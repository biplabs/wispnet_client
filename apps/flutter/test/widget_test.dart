import 'dart:async';
import 'dart:io';
import 'dart:ui' as ui;

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:wispnet/src/app.dart';
import 'package:wispnet/src/backend.dart';
import 'package:wispnet/src/cards.dart';
import 'package:wispnet/src/file_shares.dart';
import 'package:wispnet/src/space.dart';
import 'package:wispnet/src/service_settings.dart';
import 'package:wispnet/src/sheets.dart' show ConnectionDiagnostics;
import 'package:wispnet/src/terminal_sheet.dart';
import 'package:wispnet/ui.dart';

final peer = 'a' * 64;
final incarnation = 'c' * 64;

class AndroidLocalFake extends DesktopFake {
  AndroidLocalFake() {
    (status['host']['services'] as List).add({
      'id': 'incoming',
      'kind': 'file_inbox',
      'incarnation': incarnation,
      'enabled': true,
      'allow': [],
    });
  }
  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    if (method == 'folderServicesStatus') {
      return {
        'revision': status['host']['revision'],
        'clients': status['host']['clients'],
        'shares': [],
        'inboxes': [
          for (final s in status['host']['services'])
            if (s['kind'] == 'file_inbox')
              {
                ...s as Map<String, dynamic>,
                'folder': 'opaque-incoming',
                'label': 'Incoming folder',
                'available': true,
              },
        ],
      };
    }
    final result = await super.invoke(method, arguments);
    return method == 'snapshot' ? {...result, 'desktop': false} : result;
  }
}

class DesktopDiagnosticFake extends DesktopFake {
  bool extraRemote = false;
  String? failureHost;

  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    if (method == 'connectionDiagnostics') {
      calls.add((method, arguments));
      if (arguments['host'] == failureHost) {
        throw PlatformException(code: 'unavailable', message: 'Device offline');
      }
      return {
        'network': null,
        'vpn': null,
        'network_changed': null,
        'peers': [
          {
            'name': arguments['host'] == peer ? 'j-strix' : 'second-device',
            'report': {
              'initial_path': 'relay',
              'selected_path': 'direct',
              'rtt_ms': 15,
              'observation_ms': 10000,
              'relay_status': 'connected',
              'reason': 'Direct path verified.',
            },
          },
        ],
      };
    }
    final result = await super.invoke(method, arguments);
    if (method == 'snapshot' && extraRemote) {
      (result['hosts'] as Map)['e' * 64] = {
        'name': 'second-device',
        'state': 'available',
        'catalog': {'services': <Map<String, dynamic>>[]},
      };
    }
    return result;
  }
}

class FakeBackend implements WispnetBackend {
  FakeBackend({
    this.paired = true,
    this.enabled = true,
    this.state = 'available',
    this.diagnosticReport,
    this.publicAccess,
    this.accessReady,
    this.shellService = false,
    this.minimizedShell,
    this.minimizedShells,
  });
  final Map<String, dynamic>? diagnosticReport;
  final Map<String, dynamic>? publicAccess;
  Map<String, dynamic>? minimizedShell;
  List<Map<String, dynamic>>? minimizedShells;
  Future<void>? accessReady;
  bool paired, enabled;
  final bool shellService;
  final String state;
  final calls = <(String, Map<String, dynamic>)>[];
  bool failPairing = false;
  bool missingRoute = false, failOpen = false;
  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    calls.add((method, arguments));
    switch (method) {
      case 'qrCapabilities':
        return {'camera': true, 'import': true};
      case 'snapshot':
        return {
          'name': 'hibreak',
          'peer': 'd' * 64,
          'enabled': enabled,
          'relay': 'connected',
          'network': 'Network available',
          'version': '0.1.0',
          'order': {},
          'hosts': {
            for (final shell in minimizedShells ?? <Map<String, dynamic>>[])
              if (shell['host'] != peer)
                shell['host'] as String: {
                  'name': shell['host_name'],
                  'state': 'available',
                  'catalog': {'services': <Map<String, dynamic>>[]},
                },
            if (paired)
              peer: {
                'name': 'j-strix',
                'state': state,
                'status': state == 'available'
                    ? 'Available'
                    : 'Access denied by this host',
                if (publicAccess != null) 'public_access': publicAccess,
                'catalog': {
                  'services': [
                    {
                      'id': shellService ? 'shell' : 'website',
                      'display_name': 'Website',
                      'kind': shellService ? 'shell' : 'http',
                      'incarnation': incarnation,
                    },
                    {
                      'id': 'llama',
                      'display_name': 'Local model',
                      'kind': 'http',
                      'incarnation': incarnation,
                    },
                  ],
                },
              },
          },
          'telemetry': {
            'peers': {
              peer: {'path': 'direct', 'rtt_ms': 15},
            },
          },
          if (minimizedShell != null) 'shell': minimizedShell,
          if (minimizedShells != null) 'shells': minimizedShells,
        };
      case 'connectionDiagnostics':
        return {
          'network': 'Cellular',
          'vpn': false,
          'peers': [
            {
              'name': 'j-strix',
              'report':
                  diagnosticReport ??
                  {
                    'initial_path': 'relay',
                    'selected_path': 'relay',
                    'rtt_ms': 42,
                    'observation_ms': 10000,
                    'relay_status': 'connected',
                    'reason':
                        'No direct path was established within 10 seconds.',
                    'network_report': null,
                    'transport': {
                      'relays': [
                        {
                          'url': 'https://relay.wispnet.app/',
                          'connected': true,
                          'preferred': true,
                        },
                        {
                          'url': 'https://usw1-1.relay.n0.iroh.link/',
                          'connected': true,
                          'preferred': false,
                        },
                      ],
                      'discovery': [
                        {
                          'url': 'https://relay.wispnet.app/',
                          'ipv4': true,
                          'ipv6': false,
                        },
                        {
                          'url': 'https://usw1-1.relay.n0.iroh.link/',
                          'ipv4': true,
                          'ipv6': false,
                        },
                      ],
                    },
                  },
            },
          ],
        };
      case 'setAccess':
        if (arguments['enabled'] == true) await accessReady;
        enabled = arguments['enabled'] as bool;
        return {};
      case 'open':
        if (failOpen) {
          throw PlatformException(
            code: 'unavailable',
            message: 'Host unavailable',
          );
        }
        return {'url': 'http://website.wispnet.localhost:18300'};
      case 'settings':
        return {
          'port': 18400,
          'url': arguments['host'] == null
              ? 'http://wispnet.localhost:18400/registry'
              : missingRoute
              ? ''
              : 'http://j-strix-aaaaaaaaaaaa-${arguments['service']}.localhost:18400/',
        };
      case 'pair':
        if (failPairing) {
          throw PlatformException(
            code: 'invitation_denied',
            message: 'Invalid invitation',
          );
        }
        paired = true;
        return {};
      case 'removeHost':
        paired = false;
        return {};
      case 'closeShell':
        final host = arguments['host'] as String?;
        if (host != null && minimizedShells != null) {
          minimizedShells = minimizedShells!
              .where((s) => s['host'] != host)
              .toList();
        } else {
          minimizedShells = null;
        }
        minimizedShell = null;
        return {};
      case 'shellOpen':
        return {
          'session': 'handle-${arguments['session'] ?? 'main'}',
          'latency_ms': 9,
          'path': 'direct',
          'events': const [],
        };
      case 'nativeTerminalOpen':
      case 'nativeTerminalReconnect':
      case 'nativeTerminalStatus':
        return {
          'state': 'attached',
          'latency_ms': 9,
          'path': 'direct',
          'echo_enabled': true,
          'canonical': true,
        };
      case 'shellPoll':
        return {
          'state': 'attached',
          'path': 'direct',
          'latency_ms': 9,
          'events': const [
            {'type': 'output', 'data_b64': 'aGVsbG8gd29ybGQ='},
          ],
        };
      default:
        return {};
    }
  }
}

class UploadDockFake extends FakeBackend {
  UploadDockFake()
    : super(
        shellService: true,
        minimizedShell: {
          'host': peer,
          'host_name': 'j-strix',
          'service': 'shell',
          'incarnation': incarnation,
          'session': 'main',
          'title': 'j-strix · fish',
          'state': 'attached',
          'minimized': true,
        },
      );
  final uploads = <String, Map<String, dynamic>>{};

  void addUpload(
    String id,
    String host,
    String name, {
    String state = 'running',
  }) {
    uploads[id] = {
      'transfer': id,
      'host': host,
      'host_name': host == peer ? 'j-strix' : 'j-desktop',
      'incarnation': incarnation,
      'workspace': '',
      'local_path': 'content://documents/$id',
      'name': name,
      'state': state,
      'phase': 'transferring',
      'verified_bytes': 512 * 1024,
      'size_bytes': 1024 * 1024,
    };
  }

  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    if (method == 'fileList') return {'transfers': uploads.values.toList()};
    if (method == 'fileStatus' || method == 'fileCancel') {
      calls.add((method, Map.of(arguments)));
      final upload = uploads[arguments['transfer']]!;
      if (method == 'fileCancel') upload['state'] = 'cancelled';
      return Map.of(upload);
    }
    return super.invoke(method, arguments);
  }
}

class LinkFake extends FakeBackend {
  LinkFake(this.link) : super(enabled: false);
  String? link;
  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    if (method == 'takeLink') {
      final value = link;
      link = null;
      return value == null ? {} : {'link': value};
    }
    if (method == 'resolveLink') {
      calls.add((method, arguments));
      return {'url': 'http://j-strix-xxx-website.localhost:7842/'};
    }
    return super.invoke(method, arguments);
  }
}

class DesktopFake extends FakeBackend {
  final ownerCalls = <Map<String, dynamic>>[];
  String? pickedFolder = '/srv/receiving';
  Object? folderPickerError;
  final status = <String, dynamic>{
    'host': {
      'host': 'd' * 64,
      'revision': 7,
      'services': [
        {
          'id': 'web',
          'display_name': 'Shared web',
          'kind': 'http',
          'backend': '127.0.0.1:8080',
          'incarnation': incarnation,
          'allow': [peer],
        },
        {
          'id': 'shell',
          'display_name': 'Shell',
          'kind': 'shell',
          'backend': '127.0.0.1:0',
          'incarnation': incarnation,
          'allow': <String>[],
        },
      ],
      'clients': [
        {'peer': peer, 'name': 'Existing device'},
      ],
    },
    'host_name': 'Desktop',
    'connected_peers': {},
    'invitations': [
      {
        'name': 'New device',
        'peer': peer,
        'invitation': 'request-id',
        'state': 'pending',
      },
    ],
    'public_access': {'services': []},
  };
  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    if (method == 'pickFolder') {
      calls.add((method, arguments));
      if (folderPickerError case final Object error) throw error;
      return {if (pickedFolder != null) 'path': pickedFolder};
    }
    if (method == 'qrCapabilities') return {'camera': false, 'import': true};
    if (method == 'settings' && arguments['host'] == 'd' * 64) {
      calls.add((method, arguments));
      return {'port': '', 'url': 'http://shared-web.localhost:18000/'};
    }
    if (method == 'setServiceEnabled') {
      calls.add((method, arguments));
      final service = (status['host']['services'] as List).singleWhere(
        (s) => s['id'] == arguments['service'],
      );
      service['enabled'] = arguments['enabled'];
      status['host']['revision']++;
      return {};
    }
    if (method == 'ownerStatus') return status;
    if (method == 'ownerData') {
      return {
        'value': arguments['op'] == 'targets'
            ? [
                {'host': 'd' * 64},
              ]
            : [],
      };
    }
    if (method == 'ownerRequest') {
      ownerCalls.add(arguments['payload'] as Map<String, dynamic>);
      final operation = arguments['payload']['operation'];
      if (operation?['op'] == 'shell') {
        final shell = (status['host']['services'] as List).singleWhere(
          (s) => s['id'] == 'shell',
        );
        shell['allow'] = operation['allow'];
        status['host']['revision']++;
      }
      if (operation?['op'] == 'edit_service') {
        final service = (status['host']['services'] as List).singleWhere(
          (s) => s['id'] == operation['id'],
        );
        service['id'] = operation['new_id'];
        service['backend'] = operation['backend'];
        service['kind'] = operation['kind'];
        service['allow'] = operation['allow'];
        status['host']['revision']++;
      }
      if (arguments['payload']['op'] == 'invite_qr') {
        return {
          'host': 'd' * 64,
          'code': 'wispnet1:generated-code',
          'matrix': [
            [1, 0],
            [0, 1],
          ],
          'invitation': 'issued-id',
          'expires_at': DateTime.now().millisecondsSinceEpoch ~/ 1000 + 60,
        };
      }
      return {};
    }
    final reply = await super.invoke(method, arguments);
    if (method == 'snapshot') {
      return {
        ...reply,
        'desktop': true,
        'pendingRequests': 1,
        'hosts': {
          ...reply['hosts'] as Map<String, dynamic>,
          'd' * 64: {
            'name': 'hibreak',
            'local': true,
            'state': 'available',
            'status': 'This machine',
            'catalog': {
              'services': [
                for (final service in status['host']['services'])
                  {
                    ...service as Map<String, dynamic>,
                    for (final p in status['public_access']['services'])
                      if (p['service'] == service['id'] &&
                          p['incarnation'] == service['incarnation'])
                        'public_access': p,
                  },
              ],
            },
          },
        },
      };
    }
    return reply;
  }
}

DesktopFake filesBackend({
  Map<String, String> exports = const {},
  List<String> allow = const [],
}) {
  final backend = DesktopFake();
  (backend.status['host']['services'] as List).add({
    'id': 'files',
    'display_name': 'Files',
    'kind': 'files',
    'backend': '127.0.0.1:0',
    'incarnation': incarnation,
    'allow': allow,
  });
  backend.status['host']['file_exports'] = exports;
  return backend;
}

Future<void> mount(
  WidgetTester tester,
  FakeBackend backend, {
  Size size = const Size(390, 844),
  double scale = 1,
  bool disableAnimations = true,
}) async {
  tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
    SystemChannels.platform_views,
    (call) async {
      if (call.method == 'create') return 1;
      if (call.method == 'resize') {
        final args = call.arguments as Map;
        return {'width': args['width'], 'height': args['height']};
      }
      return null;
    },
  );
  addTearDown(
    () => tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
      SystemChannels.platform_views,
      null,
    ),
  );
  tester.view.devicePixelRatio = 1;
  tester.view.physicalSize = size;
  tester.platformDispatcher.accessibilityFeaturesTestValue =
      FakeAccessibilityFeatures(disableAnimations: disableAnimations);
  addTearDown(() {
    tester.view.resetPhysicalSize();
    tester.view.resetDevicePixelRatio();
    tester.platformDispatcher.clearAccessibilityFeaturesTestValue();
  });
  await tester.pumpWidget(
    RepaintBoundary(
      key: const ValueKey('capture'),
      child: MediaQuery(
        data: MediaQueryData(
          size: size,
          textScaler: TextScaler.linear(scale),
          disableAnimations: disableAnimations,
        ),
        child: WispnetApp(backend: backend),
      ),
    ),
  );
  await tester.pump();
  await tester.pump(const Duration(milliseconds: 300));
}

Future<void> close(WidgetTester tester) async {
  await tester.pumpWidget(const SizedBox());
  await tester.pump();
}

Future<void> screenshot(WidgetTester tester, String name) async {
  final folder = Platform.environment['WISPNET_SCREENSHOTS'];
  if (folder == null) return;
  await tester.runAsync(() async {
    final boundary = tester.firstRenderObject<RenderObject>(
      find.byKey(const ValueKey('capture')),
    ) as RenderRepaintBoundary;
    final image = await boundary.toImage();
    final bytes = await image.toByteData(format: ui.ImageByteFormat.png);
    final directory = Directory(folder)..createSync(recursive: true);
    File('${directory.path}/$name.png')
        .writeAsBytesSync(bytes!.buffer.asUint8List());
    image.dispose();
  });
}

void main() {
  testWidgets(
    'service links enable access and launch the resolved HTTP URL once',
    (tester) async {
      final backend = LinkFake('wispnet://$peer/website');
      await tester.pumpWidget(WispnetApp(backend: backend));
      for (var n = 0; n < 20; n++) {
        await tester.pump(const Duration(milliseconds: 100));
      }
      expect(
        backend.calls.where((c) => c.$1 == 'resolveLink').single.$2['link'],
        'wispnet://$peer/website',
      );
      expect(
        backend.calls.where((c) => c.$1 == 'launch').single.$2['url'],
        'http://j-strix-xxx-website.localhost:7842/',
      );
      expect(backend.enabled, true);
      await tester.pumpWidget(const SizedBox());
    },
  );
  testWidgets(
    'malformed service links never enable access or resolve a service',
    (tester) async {
      final backend = LinkFake('wispnet://$peer/website?backend=evil');
      await tester.pumpWidget(WispnetApp(backend: backend));
      for (var n = 0; n < 20; n++) {
        await tester.pump(const Duration(milliseconds: 100));
      }
      expect(
        backend.calls.where(
          (c) => ['setAccess', 'resolveLink', 'launch'].contains(c.$1),
        ),
        isEmpty,
      );
      expect(backend.enabled, false);
      await tester.pumpWidget(const SizedBox());
    },
  );
  testWidgets('empty remote card is compact and names the device once', (
    tester,
  ) async {
    await tester.pumpWidget(
      MaterialApp(
        home: Scaffold(
          body: SingleChildScrollView(
            child: SizedBox(
              width: 900,
              child: MachineCard(
                machine: Machine(peer, {
                  'name': 'Pixel 10 Pro',
                  'state': 'available',
                  'catalog': {'services': []},
                }),
                order: const {},
                telemetry: 'Direct · 8ms',
                stateColor: Colors.white,
                busy: false,
                onManage: () {},
                onOpen: (_) {},
                onLinks: (_) {},
                onSettings: (_) {},
                onReorder: (_, _, _, _) {},
              ),
            ),
          ),
        ),
      ),
    );
    expect(find.text('No shared services'), findsOneWidget);
    expect(find.textContaining('Pixel 10 Pro'), findsOneWidget);
    expect(find.textContaining('PIXEL 10 PRO'), findsNothing);
    expect(tester.getSize(find.byType(MachineCard)).height, lessThan(80));
  });

  testWidgets('mouse hover highlights custom controls and clears on exit', (
    tester,
  ) async {
    final backend = FakeBackend();
    String? copied;
    tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
      SystemChannels.platform,
      (call) async {
        if (call.method == 'Clipboard.setData') {
          copied = call.arguments['text'] as String;
        }
        return null;
      },
    );
    addTearDown(
      () => tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
        SystemChannels.platform,
        null,
      ),
    );
    await mount(tester, backend, size: const Size(1000, 850));
    final mouse = await tester.createGesture(kind: ui.PointerDeviceKind.mouse);
    await mouse.addPointer(location: Offset.zero);
    final controls = [
      find.descendant(
        of: find.byType(OnlineButton),
        matching: find.byType(WispInkWell),
      ),
      find.ancestor(
        of: find.text('website'),
        matching: find.byType(WispInkWell),
      ),
      find.ancestor(
        of: find.textContaining('j-strix'),
        matching: find.byType(WispInkWell),
      ),
    ];
    Color overlay(Finder control) =>
        (tester
                    .widget<AnimatedContainer>(
                      find
                          .descendant(
                            of: control,
                            matching: find.byType(AnimatedContainer),
                          )
                          .first,
                    )
                    .foregroundDecoration!
                as ShapeDecoration)
            .color!;
    for (final control in controls) {
      expect(overlay(control).a, 0);
      await mouse.moveTo(tester.getCenter(control));
      await tester.pumpAndSettle();
      expect(overlay(control).a, greaterThan(.1));
      await mouse.moveTo(Offset.zero);
      await tester.pumpAndSettle();
      expect(overlay(control).a, 0);
    }
    final heading = find.textContaining('j-strix');
    expect(
      tester.getSize(controls[2]).width,
      closeTo(tester.getSize(heading).width, .01),
    );
    final status = find.textContaining('Online · Direct');
    await mouse.moveTo(tester.getCenter(status));
    await tester.pumpAndSettle();
    expect(overlay(controls[2]).a, 0);
    final service = controls[1];
    expect(find.byTooltip('Settings for website on j-strix'), findsNothing);
    final copy = find.byTooltip('Show links for website');
    expect(tester.getRect(service).contains(tester.getCenter(copy)), isTrue);
    await mouse.moveTo(tester.getCenter(copy));
    await tester.pumpAndSettle();
    expect(overlay(service).a, greaterThan(.1));
    await mouse.down(tester.getCenter(copy));
    await mouse.up();
    await tester.pumpAndSettle();
    expect(find.byTooltip('Close'), findsOneWidget);
    expect(copied, isNull);
    expect(backend.calls.where((call) => call.$1 == 'settings'), hasLength(1));
    expect(backend.calls.where((call) => call.$1 == 'open'), isEmpty);
    await mouse.removePointer();
    await close(tester);
  });

  setUpAll(() async {
    final loader = FontLoader('Roboto');
    loader.addFont(rootBundle.load('assets/fonts/Roboto-Regular.ttf'));
    loader.addFont(rootBundle.load('assets/fonts/Roboto-Medium.ttf'));
    loader.addFont(rootBundle.load('assets/fonts/Roboto-Bold.ttf'));
    await loader.load();
    await (FontLoader(
      'MaterialIcons',
    )..addFont(rootBundle.load('fonts/MaterialIcons-Regular.otf'))).load();
  });
  testWidgets(
    'existing machines and service handoff keep host and incarnation',
    (tester) async {
      final backend = FakeBackend();
      await mount(tester, backend);
      expect(find.text('REMOTE SERVICES'), findsOneWidget);
      expect(find.text('Online · Direct · 15ms'), findsOneWidget);
      await screenshot(tester, 'android-directory');
      await tester.tap(find.textContaining('website', findRichText: true));
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 1000));
      final opened = backend.calls.where((c) => c.$1 == 'open').single.$2;
      expect(opened, {
        'host': peer,
        'service': 'website',
        'incarnation': incarnation,
      });
      expect(
        backend.calls.where((c) => c.$1 == 'launch').single.$2['url'],
        contains('website.wispnet.localhost'),
      );
      await close(tester);
    },
  );
  testWidgets('service cards prefer a published public URL', (tester) async {
    final backend = FakeBackend(
      publicAccess: {
        'version': 1,
        'host': peer,
        'services': [
          {
            'service': 'website',
            'incarnation': incarnation,
            'url': 'https://shared-website.wispnet.test/',
            'state': 'online',
          },
        ],
      },
    );
    await mount(tester, backend);
    await tester.tap(find.text('website'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 1000));
    expect(backend.calls.where((c) => c.$1 == 'open'), isEmpty);
    expect(
      backend.calls.where((c) => c.$1 == 'launch').single.$2['url'],
      'https://shared-website.wispnet.test/',
    );
    await close(tester);
  });
  testWidgets('settings follows the device label in the directory', (
    tester,
  ) async {
    for (final width in [320.0, 390.0, 1440.0]) {
      await mount(tester, FakeBackend(), size: Size(width, 844));
      final label = tester.getRect(
        find.byKey(const ValueKey('device-heading')),
      );
      final settings = tester.getRect(find.byTooltip('Settings'));
      expect(settings.left, closeTo(label.right, .01));
      expect(settings.width, 44);
      final button = tester.getRect(find.byType(OnlineButton));
      expect(button.height, 44);
      expect(button.center.dy, closeTo(settings.center.dy + 2, .01));
      await tester.tap(find.byTooltip('Online'));
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 300));
      expect(find.byType(OnlineButton), findsNothing);
      expect(tester.takeException(), isNull);
      await close(tester);
    }
  });
  testWidgets('Android and desktop use the same directory and status labels', (
    tester,
  ) async {
    for (final backend in [FakeBackend(), DesktopFake()]) {
      await mount(tester, backend, size: const Size(1000, 850));
      expect(find.text('REMOTE SERVICES'), findsOneWidget);
      expect(find.text('PAIRED MACHINES'), findsNothing);
      expect(find.byTooltip('Online'), findsOneWidget);
      expect(find.byTooltip('Go offline'), findsNothing);
      await tester.tap(find.byTooltip('Online'));
      await tester.pumpAndSettle();
      expect(backend.enabled, false);
      expect(find.byTooltip('Enter wisp space'), findsOneWidget);
      await close(tester);
    }
  });
  testWidgets('entry reveals all controls only during the end of the jump', (
    tester,
  ) async {
    await mount(tester, FakeBackend(enabled: false), disableAnimations: false);
    await tester.tap(find.byTooltip('Enter wisp space'));
    await tester.pump();
    double opacity() => tester
        .widget<FadeTransition>(find.byKey(const ValueKey('entry-reveal')))
        .opacity
        .value;
    expect(opacity(), 0);
    await tester.pump(const Duration(milliseconds: 650));
    expect(opacity(), 0);
    await tester.pump(const Duration(milliseconds: 150));
    expect(opacity(), closeTo(.5, .01));
    await tester.pump(const Duration(milliseconds: 100));
    expect(opacity(), 1);
    expect(find.text('REMOTE SERVICES'), findsOneWidget);
    await close(tester);
  });
  testWidgets('entry starts immediately while access is still starting', (
    tester,
  ) async {
    for (final startupMs in [400, 1200]) {
      final ready = Completer<void>();
      final backend = FakeBackend(enabled: false, accessReady: ready.future);
      await mount(tester, backend, disableAnimations: false);
      final entry = find.byTooltip('Enter wisp space');
      await tester.tap(entry);
      await tester.pump();
      final warp = tester.widget<WispSpace>(find.byType(WispSpace)).warp;
      await tester.pump(const Duration(milliseconds: 100));
      expect(warp.value, closeTo(1 / 9, .01));
      await tester.tap(entry, warnIfMissed: false);
      expect(backend.calls.where((c) => c.$1 == 'setAccess'), hasLength(1));
      await tester.pump(Duration(milliseconds: startupMs - 100));
      expect(backend.enabled, false);
      expect(
        tester
            .widget<FadeTransition>(find.byKey(const ValueKey('entry-reveal')))
            .opacity
            .value,
        0,
      );
      ready.complete();
      await tester.pump();
      if (startupMs < 900) {
        await tester.pump(Duration(milliseconds: 800 - startupMs));
        expect(
          tester
              .widget<FadeTransition>(
                find.byKey(const ValueKey('entry-reveal')),
              )
              .opacity
              .value,
          closeTo(.5, .01),
        );
        await tester.pump(const Duration(milliseconds: 100));
      }
      await tester.pump(const Duration(milliseconds: 1));
      expect(warp.value, 0, reason: 'startup took $startupMs ms');
      expect(find.text('REMOTE SERVICES'), findsOneWidget);
      expect(
        tester.widget<OnlineButton>(find.byType(OnlineButton)).onPressed,
        isNotNull,
      );
      await close(tester);
    }
  });
  testWidgets('failed entry stops the jump and allows an immediate retry', (
    tester,
  ) async {
    final ready = Completer<void>();
    final backend = FakeBackend(enabled: false, accessReady: ready.future);
    await mount(tester, backend, disableAnimations: false);
    await tester.tap(find.byTooltip('Enter wisp space'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 100));
    ready.completeError(
      PlatformException(code: 'access_failed', message: 'Cannot start access'),
    );
    await tester.pump();
    expect(tester.widget<WispSpace>(find.byType(WispSpace)).warp.value, 0);
    expect(find.text('Cannot start access'), findsOneWidget);
    backend.accessReady = null;
    await tester.tap(find.byTooltip('Enter wisp space'));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 900));
    expect(find.text('REMOTE SERVICES'), findsOneWidget);
    expect(tester.takeException(), isNull);
    await close(tester);
  });
  testWidgets('both launch states show a centered entry and its caption', (
    tester,
  ) async {
    for (final paired in [false, true]) {
      final backend = FakeBackend(paired: paired, enabled: false);
      await mount(tester, backend);
      final caption = find.text(
        paired ? 'hyperjump to wispnet' : 'establish your wispnet',
      );
      expect(caption, findsOneWidget);
      expect(find.byType(Text), findsOneWidget);
      expect(find.byTooltip('Settings'), findsNothing);
      expect(find.text('Manage'), findsNothing);
      final entry = find.byTooltip(
        paired ? 'Enter wisp space' : 'Establish your wisp network',
      );
      expect(tester.getCenter(entry).dx, 195);
      final group = tester
          .getRect(entry)
          .expandToInclude(tester.getRect(caption));
      expect(group.center.dy, closeTo(422, .01));
      await tester.tap(entry);
      await tester.pumpAndSettle();
      expect(
        paired ? find.text('REMOTE SERVICES') : find.text('Pair a machine'),
        findsOneWidget,
      );
      await close(tester);
    }
  });
  testWidgets('Online status caption uses natural width at phone density', (
    tester,
  ) async {
    tester.view.devicePixelRatio = 1.875;
    tester.view.physicalSize = const Size(1072, 1448);
    addTearDown(() {
      tester.view.resetPhysicalSize();
      tester.view.resetDevicePixelRatio();
    });
    await tester.pumpWidget(
      MaterialApp(
        home: Material(
          child: Center(child: OnlineButton(onPressed: () {})),
        ),
      ),
    );
    await tester.pump();
    final bounds = tester.getSize(find.byType(OnlineButton));
    final caption = OnlineButton.labelPainter();
    expect(caption.text!.toPlainText(), 'Online');
    expect(
      (bounds.width * 1.875).round(),
      ((caption.width + 48) * 1.875).ceil(),
    );
    caption.dispose();
    expect(find.byTooltip('Online'), findsOneWidget);
    expect((bounds.height * 1.875).round(), 83);
    final folder = Platform.environment['WISPNET_SCREENSHOTS'];
    if (folder != null) {
      await tester.runAsync(() async {
        final boundary = tester.renderObject<RenderRepaintBoundary>(
          find.byKey(const ValueKey('offline-control')),
        );
        final image = await boundary.toImage(pixelRatio: 1.875);
        final bytes = await image.toByteData(format: ui.ImageByteFormat.png);
        Directory(folder).createSync(recursive: true);
        File('$folder/flutter-offline-toggle.png')
            .writeAsBytesSync(bytes!.buffer.asUint8List());
        image.dispose();
      });
    }
    await close(tester);
  });
  testWidgets('desktop pairing uses the shared QR and code form', (
    tester,
  ) async {
    await mount(tester, DesktopFake(), size: const Size(1000, 850));
    await tester.tap(
      find
          .descendant(
            of: find.byKey(const ValueKey('remote-sector')),
            matching: find.byType(OutlinedButton),
          )
          .first,
    );
    await tester.pumpAndSettle();
    await tester.tap(find.text('+ Pair another machine'));
    await tester.pumpAndSettle();
    expect(find.text('Pair a machine'), findsOneWidget);
    expect(find.text('Pairing Code'), findsOneWidget);
    expect(find.text('QR'), findsOneWidget);
    expect(find.text('Import QR image'), findsOneWidget);
    expect(find.text('Scan with camera'), findsNothing);
    await tester.tap(find.text('Pairing Code'));
    await tester.pumpAndSettle();
    expect(find.byType(TextField), findsOneWidget);
    final pair = find.widgetWithText(FilledButton, 'Pair');
    expect(tester.widget<FilledButton>(pair).onPressed, isNull);
    await tester.enterText(find.byType(TextField), 'wispnet1:samplecode');
    await tester.pump();
    expect(tester.widget<FilledButton>(pair).onPressed, isNotNull);
    await close(tester);
  });

  testWidgets('denied cached services never issue an open request', (
    tester,
  ) async {
    final backend = FakeBackend(state: 'host_denied');
    await mount(tester, backend);
    await tester.tap(find.textContaining('website', findRichText: true));
    await tester.pump();
    expect(backend.calls.where((c) => c.$1 == 'open'), isEmpty);
    await close(tester);
  });
  testWidgets(
    'pairing disables blank input and retains failed code and feedback',
    (tester) async {
      final backend = FakeBackend(paired: false, enabled: false)
        ..failPairing = true;
      await mount(tester, backend);
      await screenshot(tester, 'android-standby');
      await tester.tap(find.byTooltip('Establish your wisp network'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Pairing Code'));
      await tester.pumpAndSettle();
      final pair = find.widgetWithText(FilledButton, 'Pair');
      expect(tester.widget<FilledButton>(pair).onPressed, isNull);
      await tester.enterText(find.byType(TextField), '   \n ');
      await tester.pump();
      expect(tester.widget<FilledButton>(pair).onPressed, isNull);
      await tester.enterText(find.byType(TextField), 'wispnet1:sample\ncode');
      await tester.pump();
      await tester.tap(pair);
      await tester.pumpAndSettle();
      expect(find.textContaining('This host did not accept'), findsOneWidget);
      expect(
        tester.widget<TextField>(find.byType(TextField)).controller!.text,
        'wispnet1:sample\ncode',
      );
      await screenshot(tester, 'android-pairing');
      await close(tester);
    },
  );
  testWidgets('dragging a service saves its order within its machine', (
    tester,
  ) async {
    final backend = FakeBackend();
    await mount(tester, backend);
    final gesture = await tester.startGesture(
      tester.getCenter(find.text('website')),
    );
    await tester.pump(const Duration(milliseconds: 650));
    await gesture.moveTo(tester.getCenter(find.text('llama')));
    await tester.pump();
    await gesture.up();
    await tester.pump();
    final saved = backend.calls.where((c) => c.$1 == 'saveOrder').single.$2;
    expect(saved, {
      'group': 'services/$peer',
      'ids': ['llama', 'website'],
    });
    await close(tester);
  });
  testWidgets('desktop service cards reorder without a long press', (
    tester,
  ) async {
    final backend = FakeBackend();
    debugDefaultTargetPlatformOverride = TargetPlatform.linux;
    await mount(tester, backend);
    final gesture = await tester.startGesture(
      tester.getCenter(find.text('website')),
    );
    await gesture.moveBy(const Offset(0, 20));
    await tester.pump();
    await gesture.moveTo(tester.getCenter(find.text('llama')));
    await tester.pump();
    await gesture.up();
    await tester.pump();
    expect(backend.calls.where((c) => c.$1 == 'saveOrder').single.$2, {
      'group': 'services/$peer',
      'ids': ['llama', 'website'],
    });
    await close(tester);
    debugDefaultTargetPlatformOverride = null;
  });
  testWidgets('Diagnostic below Save opens a page and runs automatically', (
    tester,
  ) async {
    final backend = FakeBackend();
    await mount(tester, backend);
    expect(find.text('Diagnostic'), findsNothing);
    await tester.tap(find.byTooltip('Settings'));
    await tester.pumpAndSettle();
    expect(find.byTooltip('Connection diagnostics'), findsNothing);
    expect(
      tester.getTopLeft(find.text('Diagnostic')).dy,
      greaterThan(tester.getBottomLeft(find.text('Save').last).dy),
    );
    await tester.tap(find.text('Diagnostic'));
    await tester.pumpAndSettle();
    expect(find.widgetWithText(AppBar, 'Diagnostic'), findsOneWidget);
    expect(find.textContaining('Network type: Cellular'), findsOneWidget);
    expect(find.textContaining('relay → relay'), findsOneWidget);
    expect(find.textContaining('Device relay connections'), findsOneWidget);
    expect(
      find.textContaining('✓ https://relay.wispnet.app/ · preferred'),
      findsOneWidget,
    );
    expect(
      find.textContaining('✓ https://usw1-1.relay.n0.iroh.link/'),
      findsOneWidget,
    );
    expect(
      find.textContaining('Device discovery observations'),
      findsOneWidget,
    );
    expect(find.textContaining('• ? UDP response IPv4'), findsOneWidget);
    expect(
      find.textContaining('Automatic router mapping status unavailable'),
      findsOneWidget,
    );
    expect(
      backend.calls
          .where((c) => c.$1 == 'connectionDiagnostics')
          .single
          .$2['host'],
      peer,
    );
    expect(backend.calls.where((c) => c.$1 == 'saveSettings'), isEmpty);
    await tester.pageBack();
    await tester.pumpAndSettle();
    expect(find.widgetWithText(TextField, 'Device ID'), findsOneWidget);
    expect(find.text('Save').last, findsOneWidget);
  });
  testWidgets('both ends distinguish refreshed discovery from cached reports', (
    tester,
  ) async {
    Map<String, dynamic> side(bool fresh) => {
      'relay_status': 'connected',
      'elapsed_ms': 20000,
      'peer_ip_candidates': 2,
      'network': {
        'fresh': fresh,
        'wait_ms': 20000,
        'report': {
          'udp_v4': true,
          'udp_v6': false,
          'mapping_varies': null,
          'public_ipv4_discovered': fresh,
          'public_ipv6_discovered': false,
        },
      },
      'paths': {
        'initial_path': 'relay',
        'selected_path': 'relay',
        'rtt_ms': 100,
        'direct_paths_seen': 0,
        'direct_paths_closed': 0,
        'direct_datagrams_sent': 0,
        'direct_datagrams_received': 0,
        'direct_challenges_sent': 0,
        'direct_responses_received': 0,
        'connection_challenges_sent': 3,
        'connection_responses_received': 2,
        'events_lost': 0,
      },
    };
    final backend = FakeBackend(
      diagnosticReport: {
        'initial_path': 'relay',
        'selected_path': 'relay',
        'observation_ms': 20000,
        'relay_status': 'connected',
        'reason': 'UDP discovery works; direct peer connectivity failed during this test.',
        'local': side(true),
        'remote': side(false),
        'router_mapping': {
          'local': {
            'enabled': true,
            'active': true,
            'external_port': 54321,
            'public_address': true,
            'upnp': true,
            'pcp': false,
            'nat_pmp': null,
          },
          'remote': {
            'enabled': true,
            'active': false,
            'external_port': null,
            'public_address': null,
            'upnp': true,
            'pcp': false,
            'nat_pmp': null,
          },
        },
      },
    );
    await mount(tester, backend);
    await tester.tap(find.byTooltip('Settings'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Diagnostic'));
    await tester.pumpAndSettle();
    final text = tester
        .widget<SelectableText>(find.byType(SelectableText))
        .data!;
    expect(text, contains('✓ Automatic router mapping enabled'));
    expect(text, contains('UPnP ✓ · PCP ✗ · NAT-PMP ?'));
    expect(text, contains('✓ Router mapping granted · UDP 54321'));
    expect(text, contains('✗ Router mapping granted'));
    expect(text, contains('A granted mapping does not prove reachability'));

    expect(text, contains('Device observations'));
    expect(text, contains('Remote device observations'));
    expect(text, contains('network report updated during this test'));
    expect(text, contains('cached; no new report observed during this test'));
    expect(text, contains('across all paths (includes relay)'));
    expect(text, contains('Known peer IP addresses in transport cache: 2'));
    expect(text, contains('cache count does not measure address exchange'));
    expect(text, contains('Discovery responded, but no public IP was found'));
    expect(
      text,
      contains('Zero observed paths does not prove no attempts were made'),
    );
  });
  testWidgets(
    'desktop Diagnostic uses the shared page and skips the local device',
    (tester) async {
      debugDefaultTargetPlatformOverride = TargetPlatform.linux;
      addTearDown(() => debugDefaultTargetPlatformOverride = null);
      final backend = DesktopDiagnosticFake();
      await mount(tester, backend, size: const Size(1000, 850));
      await tester.tap(find.byTooltip('Settings'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Diagnostic'));
      await tester.pumpAndSettle();
      expect(find.byType(ConnectionDiagnostics), findsOneWidget);
      expect(
        find.textContaining('Network type: Unavailable · VPN: ?'),
        findsOneWidget,
      );
      expect(find.textContaining('relay → direct'), findsOneWidget);
      expect(find.textContaining('Device observations'), findsOneWidget);
      expect(find.textContaining('Phone'), findsNothing);
      expect(
        backend.calls
            .where((call) => call.$1 == 'connectionDiagnostics')
            .map((call) => call.$2['host']),
        [peer],
      );
      expect(backend.ownerCalls, isEmpty);
      await close(tester);
      debugDefaultTargetPlatformOverride = null;
    },
  );
  testWidgets('diagnostics continue after one remote fails', (tester) async {
    final backend = DesktopDiagnosticFake()
      ..extraRemote = true
      ..failureHost = peer;
    await mount(tester, backend, size: const Size(1000, 850));
    await tester.tap(find.byTooltip('Settings'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Diagnostic'));
    await tester.pumpAndSettle();
    expect(
      find.textContaining('Could not verify connection: Device offline'),
      findsOneWidget,
    );
    expect(find.textContaining('Direct path verified.'), findsOneWidget);
    expect(
      backend.calls
          .where((call) => call.$1 == 'connectionDiagnostics')
          .map((call) => call.$2['host']),
      [peer, 'e' * 64],
    );
    await close(tester);
  });
  testWidgets('settings validate before changing native state', (tester) async {
    final backend = FakeBackend();
    await mount(tester, backend);
    await tester.tap(find.byTooltip('Settings'));
    await tester.pumpAndSettle();
    await tester.enterText(
      find.widgetWithText(TextField, 'Wispnet port'),
      '80',
    );
    await tester.tap(find.widgetWithText(FilledButton, 'Save'));
    await tester.pump();
    expect(find.text('Port must be between 1024 and 65535.'), findsOneWidget);
    expect(backend.calls.where((c) => c.$1 == 'saveSettings'), isEmpty);
    await tester.enterText(
      find.widgetWithText(TextField, 'Wispnet port'),
      '18401',
    );
    for (final invalid in ['has spaces', 'Uppercase', '-prefix', 'a' * 65]) {
      await tester.enterText(
        find.widgetWithText(TextField, 'Device ID'),
        invalid,
      );
      await tester.tap(find.widgetWithText(FilledButton, 'Save'));
      await tester.pump();
      expect(find.textContaining('Use 1–64 lowercase'), findsOneWidget);
      expect(backend.calls.where((c) => c.$1 == 'saveSettings'), isEmpty);
    }
    await tester.enterText(
      find.widgetWithText(TextField, 'Device ID'),
      'my-phone',
    );
    await tester.tap(find.widgetWithText(FilledButton, 'Save'));
    await tester.pumpAndSettle();
    expect(
      backend.calls.where((c) => c.$1 == 'saveSettings').single.$2['port'],
      18401,
    );
    await close(tester);
  });
  for (final size in [const Size(320, 640), const Size(1440, 900)]) {
    testWidgets('shared components fit $size', (tester) async {
      await mount(tester, FakeBackend(), size: size);
      expect(tester.takeException(), isNull);
      if (size.width > 1000) await screenshot(tester, 'desktop-components');
      await close(tester);
    });
  }
  testWidgets('phone layout supports large text and a settings sheet', (
    tester,
  ) async {
    await mount(tester, FakeBackend(), size: const Size(360, 780), scale: 2);
    expect(tester.takeException(), isNull);
    await tester.tap(find.byTooltip('Settings'));
    await tester.pumpAndSettle();
    expect(tester.takeException(), isNull);
    await close(tester);
  });
  testWidgets('paired machines show one readable ID and their host identity', (
    tester,
  ) async {
    final backend = FakeBackend();
    await mount(tester, backend);
    await tester.tap(find.textContaining('j-strix'));
    await tester.pumpAndSettle();
    expect(find.text('Device ID\nj-strix\n\nHost ID\n$peer'), findsOneWidget);
    expect(find.widgetWithText(TextField, 'Display name'), findsNothing);
    expect(find.text('Save name'), findsNothing);
    expect(backend.calls.where((call) => call.$1 == 'renameHost'), isEmpty);
    await close(tester);
  });
  testWidgets('machine settings controls have consistent spacing', (
    tester,
  ) async {
    final backend = DesktopFake();
    await mount(tester, backend, size: const Size(1000, 850));
    await tester.tap(find.textContaining('j-strix'));
    await tester.pumpAndSettle();
    expect(find.widgetWithText(OutlinedButton, 'Refresh'), findsNothing);
    expect(find.byTooltip('Refresh remote services'), findsOneWidget);
    final remove = find.widgetWithText(OutlinedButton, 'Remove');
    expect(
      tester.widget<OutlinedButton>(remove).style!.foregroundColor!.resolve({}),
      WispColors.error,
    );
    await tester.tap(remove);
    await tester.pumpAndSettle();
    expect(find.text('Remove machine?'), findsOneWidget);
    expect(
      find.textContaining(
        'forget the pairing and close its local service routes',
      ),
      findsOneWidget,
    );
    expect(backend.calls.where((call) => call.$1 == 'removeHost'), isEmpty);
    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();
    expect(backend.calls.where((call) => call.$1 == 'removeHost'), isEmpty);
    await tester.tap(remove);
    await tester.pumpAndSettle();
    await tester.tap(find.widgetWithText(FilledButton, 'Remove machine'));
    await tester.pumpAndSettle();
    expect(
      backend.calls.where((call) => call.$1 == 'removeHost').single.$2['host'],
      peer,
    );
    expect(find.widgetWithText(TextField, 'Display name'), findsNothing);
    expect(tester.takeException(), isNull);
    await close(tester);
  });
  testWidgets('desktop device gear opens name and registry settings directly', (
    tester,
  ) async {
    final backend = DesktopFake();
    String? copied;
    tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
      SystemChannels.platform,
      (call) async {
        if (call.method == 'Clipboard.setData') {
          copied = call.arguments['text'] as String;
        }
        return null;
      },
    );
    addTearDown(
      () => tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
        SystemChannels.platform,
        null,
      ),
    );
    await mount(tester, backend, size: const Size(1000, 850));
    await tester.tap(find.byTooltip('Settings'));
    await tester.pumpAndSettle();
    expect(find.widgetWithText(TextField, 'Device ID'), findsOneWidget);
    expect(find.widgetWithText(TextField, 'Wispnet port'), findsOneWidget);
    expect(find.text('Device and browser settings'), findsNothing);
    expect(find.byType(TabBar), findsNothing);
    expect(find.byType(DropdownButton<String>), findsNothing);
    expect(
      find.text('http://wispnet.localhost:18400/registry'),
      findsOneWidget,
    );
    await tester.tap(find.byKey(const ValueKey('settings-address')));
    await tester.pumpAndSettle();
    expect(
      backend.calls.where((call) => call.$1 == 'launch').single.$2['url'],
      'http://wispnet.localhost:18400/registry',
    );
    await tester.ensureVisible(
      find.byKey(const ValueKey('settings-mcp-address')),
    );
    await tester.tap(find.byKey(const ValueKey('settings-mcp-address')));
    await tester.pumpAndSettle();
    expect(
      backend.calls.where((call) => call.$1 == 'launch').last.$2['url'],
      'http://wispnet.localhost:18400/mcp',
    );
    for (final (key, url) in [
      ('settings-registry-copy', 'http://wispnet.localhost:18400/registry'),
      ('settings-mcp-copy', 'http://wispnet.localhost:18400/mcp'),
    ]) {
      final button = find.byKey(ValueKey(key));
      await tester.ensureVisible(button);
      await tester.tap(button);
      await tester.pumpAndSettle();
      expect(copied, url);
    }
    expect(find.byTooltip('Close'), findsOneWidget);
    await tester.tap(find.byTooltip('Close').last);
    await tester.pumpAndSettle();
    expect(find.byTooltip('Close'), findsNothing);
    await close(tester);
  });
  testWidgets('new service accepts one explicit ID and saves a numeric port', (
    tester,
  ) async {
    final backend = DesktopFake();
    await mount(tester, backend, size: const Size(1000, 850));
    await tester.tap(find.byTooltip('Settings'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Local services'));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const ValueKey('share-service')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Custom'));
    await tester.pumpAndSettle();
    final id = find.widgetWithText(TextField, 'Service ID');
    final port = find.widgetWithText(TextField, 'Port');
    expect(find.widgetWithText(TextField, 'Service name'), findsNothing);
    expect(tester.getRect(id).top, greaterThan(tester.getRect(port).bottom));
    expect(tester.widget<TextField>(port).controller!.text, '8080');
    for (final invalid in ['', 'UpperCase', 'has spaces', 'a' * 65]) {
      await tester.enterText(id, invalid);
      await tester.ensureVisible(find.text('Save').last);
      await tester.tap(find.text('Save').last);
      await tester.pumpAndSettle();
      expect(find.text('Enter a valid service ID.'), findsOneWidget);
      expect(backend.ownerCalls, isEmpty);
    }
    await tester.enterText(id, 'local_model_api');
    for (final invalid in ['0', '65536']) {
      await tester.enterText(port, invalid);
      await tester.ensureVisible(find.text('Save').last);
      await tester.tap(find.text('Save').last);
      await tester.pumpAndSettle();
      expect(find.text('Enter a port between 1 and 65535.'), findsOneWidget);
      expect(backend.ownerCalls, isEmpty);
    }
    expect(
      tester
          .widget<DropdownButtonFormField<String>>(
            find.byType(DropdownButtonFormField<String>),
          )
          .decoration
          .labelText,
      'Protocol',
    );
    await tester.tap(find.byType(DropdownButtonFormField<String>));
    await tester.pumpAndSettle();
    await tester.tap(find.text('TCP').last);
    await tester.pumpAndSettle();
    await tester.enterText(port, '8201');
    await tester.tap(find.text('Save').last);
    await tester.pumpAndSettle();
    final operation = backend.ownerCalls.single['operation'];
    expect(operation['id'], 'local_model_api');
    expect(operation.containsKey('display_name'), isFalse);
    expect(operation['backend'], '127.0.0.1:8201');
    expect(operation['kind'], 'tcp');
    await close(tester);
  });

  testWidgets('each local service gear edits only its own service', (
    tester,
  ) async {
    final backend = DesktopFake();
    (backend.status['host']['services'] as List).add({
      'id': 'other',
      'display_name': 'Other service',
      'kind': 'http',
      'backend': '[::1]:8090',
      'incarnation': incarnation,
      'allow': [],
    });
    await mount(tester, backend, size: const Size(1000, 850));
    await tester.tap(find.byTooltip('Settings for other on hibreak'));
    await tester.pumpAndSettle();
    expect(find.text('Edit'), findsOneWidget);
    expect(find.text('Browser settings'), findsNothing);
    expect(find.byKey(const ValueKey('share-service')), findsNothing);
    await tester.tap(find.text('Edit'));
    await tester.pumpAndSettle();
    expect(
      tester
          .widget<TextField>(find.widgetWithText(TextField, 'Port'))
          .controller!
          .text,
      '8090',
    );
    expect(
      tester
          .widget<TextField>(find.widgetWithText(TextField, 'Service ID'))
          .enabled,
      true,
    );
    await tester.ensureVisible(find.text('Save').last);
    await tester.tap(find.text('Save').last);
    await tester.pumpAndSettle();
    expect(backend.ownerCalls.single['operation']['id'], 'other');
    expect(backend.ownerCalls.single['operation']['op'], 'edit_service');
    expect(backend.ownerCalls.single['operation']['new_id'], 'other');
    expect(backend.ownerCalls.single['operation']['backend'], '[::1]:8090');
    expect(backend.ownerCalls.single['operation']['allow'], isEmpty);
    await close(tester);
  });
  testWidgets('service rename retains grants and allows editing the port', (
    tester,
  ) async {
    final backend = DesktopFake();
    (backend.status['host']['services'] as List).add({
      'id': 'taken',
      'kind': 'http',
      'backend': '127.0.0.1:8090',
      'incarnation': incarnation,
      'allow': [],
    });
    await mount(tester, backend, size: const Size(1000, 850));
    await tester.tap(find.byTooltip('Settings for web on hibreak'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Edit'));
    await tester.pumpAndSettle();
    final id = find.widgetWithText(TextField, 'Service ID');
    final port = find.widgetWithText(TextField, 'Port');
    expect(tester.widget<TextField>(id).enabled, true);
    expect(tester.widget<TextField>(port).enabled, true);
    expect(
      tester.widget<WispToggleListTile>(find.byType(WispToggleListTile)).value,
      true,
    );
    for (final invalid in ['UpperCase', 'taken']) {
      await tester.enterText(id, invalid);
      await tester.ensureVisible(find.text('Save').last);
      await tester.tap(find.text('Save').last);
      await tester.pumpAndSettle();
      expect(backend.ownerCalls, isEmpty);
      expect(
        find.text(
          invalid == 'taken'
              ? 'This ID exists. Edit that service instead.'
              : 'Enter a valid service ID.',
        ),
        findsOneWidget,
      );
    }
    await tester.enterText(id, 'renamed-web');
    await tester.enterText(port, '8310');
    await tester.ensureVisible(find.text('Save').last);
    await tester.tap(find.text('Save').last);
    await tester.pumpAndSettle();
    expect(backend.ownerCalls.single, {
      'op': 'mutate',
      'operation': {
        'op': 'edit_service',
        'expected_revision': 7,
        'id': 'web',
        'new_id': 'renamed-web',
        'backend': '127.0.0.1:8310',
        'kind': 'http',
        'allow': [peer],
      },
    });
    expect(find.byType(ServiceSettings), findsNothing);
    expect(
      find.byTooltip('Settings for renamed-web on hibreak'),
      findsOneWidget,
    );
    expect(find.byTooltip('Settings for web on hibreak'), findsNothing);
    expect(tester.takeException(), isNull);
    await close(tester);
  });
  testWidgets('desktop owner tools use sheets and retain service grants', (
    tester,
  ) async {
    final backend = DesktopFake();
    await mount(tester, backend, size: const Size(1000, 850));
    await tester.tap(find.byTooltip('Settings for web on hibreak'));
    await tester.pumpAndSettle();
    expect(find.text('Device and browser settings'), findsNothing);
    expect(find.byKey(const ValueKey('share-service')), findsNothing);
    expect(find.text('Pair this machine'), findsNothing);
    expect(find.text('Browser settings'), findsNothing);
    await tester.tap(find.text('Edit'));
    await tester.pumpAndSettle();
    expect(find.widgetWithText(TextField, 'Service name'), findsNothing);
    await tester.ensureVisible(find.text('Save').last);
    await tester.tap(find.text('Save').last);
    await tester.pumpAndSettle();
    final operation = backend.ownerCalls.single['operation'] as Map;
    expect(operation['expected_revision'], 7);
    expect(operation['id'], 'web');
    expect(operation['backend'], '127.0.0.1:8080');
    expect(operation['allow'], [peer]);
    expect(operation.containsKey('display_name'), isFalse);
    expect(tester.takeException(), isNull);
    await close(tester);
  });
  testWidgets('desktop sharing saves only the service ID', (tester) async {
    final backend = DesktopFake();
    await mount(tester, backend, size: const Size(1000, 850));
    await tester.tap(find.byTooltip('Settings'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Local services'));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const ValueKey('share-service')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Custom'));
    await tester.pumpAndSettle();
    await tester.enterText(
      find.widgetWithText(TextField, 'Service ID'),
      'new-service',
    );
    await tester.ensureVisible(find.text('Save').last);
    await tester.tap(find.text('Save').last);
    await tester.pumpAndSettle();
    expect(backend.ownerCalls.single['operation']['id'], 'new-service');
    expect(backend.ownerCalls.single['operation']['allow'], isEmpty);
    expect(tester.takeException(), isNull);
    await close(tester);
  });
  testWidgets(
    'remote Manage button follows its title at phone and desktop widths',
    (tester) async {
      for (final width in [390.0, 1000.0]) {
        await mount(tester, DesktopFake(), size: Size(width, 850));
        for (final section in ['remote']) {
          final container = find.byKey(ValueKey('$section-sector'));
          final title = find.descendant(
            of: container,
            matching: find.text(
              section == 'local' ? 'LOCAL SERVICES' : 'REMOTE SERVICES',
            ),
          );
          final manage = find
              .descendant(of: container, matching: find.byType(OutlinedButton))
              .first;
          expect(
            tester.getRect(manage).left,
            closeTo(tester.getRect(title).right + 12, .01),
          );
        }
        final remote = find.byKey(const ValueKey('remote-sector'));
        final refresh = find.ancestor(
          of: find.byTooltip('Refresh remote services'),
          matching: find.byType(IconButton),
        );
        expect(
          tester.getRect(refresh).right,
          closeTo(tester.getRect(remote).right, .01),
        );
        expect(
          tester.widget<Text>(find.text('REMOTE SERVICES')).style!.fontSize,
          16,
        );
        expect(tester.takeException(), isNull);
        await close(tester);
      }
    },
  );
  testWidgets(
    'an empty local sector has no creation shortcut or unconfigured inbox',
    (tester) async {
      await mount(tester, FakeBackend(), size: const Size(600, 850));
      final sector = find.byKey(const ValueKey('local-sector'));
      expect(
        find.descendant(of: sector, matching: find.text('Add folder')),
        findsNothing,
      );
      expect(
        find.descendant(of: sector, matching: find.byType(ReorderTile)),
        findsNothing,
      );
      expect(find.byKey(const ValueKey('service-toggle-files')), findsNothing);
      expect(tester.takeException(), isNull);
      await close(tester);
    },
  );
  testWidgets('local services and remote machines have separate controls', (
    tester,
  ) async {
    final backend = DesktopFake();
    await mount(tester, backend, size: const Size(1000, 850));
    expect(find.text('Share a service'), findsNothing);
    final local = find.byKey(const ValueKey('local-sector'));
    final remote = find.byKey(const ValueKey('remote-sector'));
    expect(
      find.descendant(of: local, matching: find.text('LOCAL SERVICES')),
      findsOneWidget,
    );
    expect(
      find.descendant(of: local, matching: find.textContaining('hibreak')),
      findsNothing,
    );
    expect(
      find.descendant(of: local, matching: find.byType(WispToggle)),
      findsWidgets,
    );
    expect(
      find.descendant(of: local, matching: find.byType(WispStatusDot)),
      findsNothing,
    );
    expect(
      find.descendant(of: local, matching: find.text('web')),
      findsOneWidget,
    );
    expect(
      find.descendant(
        of: remote,
        matching: find.byTooltip('Refresh remote services'),
      ),
      findsOneWidget,
    );
    expect(
      backend.calls
          .where((c) => c.$1 == 'refreshHost')
          .every((c) => c.$2['host'] == peer),
      true,
    );
    expect(
      find.descendant(of: local, matching: find.text('Manage')),
      findsNothing,
    );
    await tester.tap(find.byTooltip('Settings'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Local services'));
    await tester.pumpAndSettle();
    expect(find.byKey(const ValueKey('share-service')), findsOneWidget);
    expect(find.text('Pair this machine'), findsNothing);
    expect(find.text('Device and browser settings'), findsNothing);
    expect(find.text('Pair with this machine'), findsNothing);
    expect(find.text('Edit'), findsNothing);
    expect(find.text('Remove service'), findsNothing);
    expect(find.text('Enable public access'), findsNothing);
    await tester.tap(find.byTooltip('Close').last);
    await tester.pumpAndSettle();
    await tester.tap(
      find.descendant(of: remote, matching: find.byType(OutlinedButton)).first,
    );
    await tester.pumpAndSettle();
    expect(find.text('Paired machines'), findsOneWidget);
    expect(find.text('hibreak · dddddddd'), findsOneWidget);
    expect(find.text('j-strix · aaaaaaaa'), findsNWidgets(2));
    expect(tester.takeException(), isNull);
    await close(tester);
  });
  testWidgets(
    'pairing remains available in Settings and opens its QR and code',
    (tester) async {
      final backend = DesktopFake();
      await tester.pumpWidget(
        MaterialApp(
          theme: wispTheme(),
          home: Scaffold(
            body: SingleChildScrollView(
              child: ServiceSettings(backend: backend, onChanged: () {}),
            ),
          ),
        ),
      );
      await tester.pumpAndSettle();
      expect(find.text('Pair this machine'), findsOneWidget);
      await tester.tap(find.text('Pair this machine'));
      await tester.pumpAndSettle();
      expect(find.byKey(const ValueKey('pairing-qr')), findsOneWidget);
      expect(find.text('Copy pairing code'), findsNothing);
      await tester.tap(find.text('Pairing code'));
      await tester.pumpAndSettle();
      expect(find.byKey(const ValueKey('pairing-code')), findsOneWidget);
      expect(find.text('wispnet1:generated-code'), findsOneWidget);
      expect(
        backend.ownerCalls.where((call) => call['op'] == 'invite_qr'),
        hasLength(1),
      );
      expect(find.text('Copy pairing code'), findsOneWidget);
      await tester.tap(find.text('QR code'));
      await tester.pumpAndSettle();
      expect(find.byKey(const ValueKey('pairing-qr')), findsOneWidget);
      await close(tester);
    },
  );
  testWidgets('service switches and Receive files module setting persist', (
    tester,
  ) async {
    final backend = filesBackend(
      exports: {'incoming': '/srv/receiving'},
      allow: [peer],
    );
    await mount(tester, backend, size: const Size(1000, 850));
    final web = find.byKey(const ValueKey('service-toggle-web'));
    expect(tester.widget<WispToggle>(web).value, isTrue);
    await tester.tap(web);
    await tester.pumpAndSettle();
    expect(tester.widget<WispToggle>(web).value, isFalse);
    await tester.tap(find.byTooltip('Settings'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Local services'));
    await tester.pumpAndSettle();
    final files = find.descendant(
      of: find.byType(ServiceSettings),
      matching: find.byWidgetPredicate(
        (w) => w is WispServiceItem && w.title == 'files',
      ),
    );
    final folder = find.descendant(
      of: files,
      matching: find.byIcon(Icons.folder_outlined),
    );
    final toggle = find.descendant(
      of: files,
      matching: find.byType(WispToggle),
    );
    final gear = find.descendant(
      of: files,
      matching: find.byTooltip('File inbox'),
    );
    expect(
      tester.getRect(folder).right,
      lessThan(
        tester
            .getRect(find.descendant(of: files, matching: find.text('files')))
            .left,
      ),
    );
    expect(
      tester.getRect(toggle).right,
      lessThanOrEqualTo(tester.getRect(gear).left),
    );
    await screenshot(tester, 'desktop-local-services');
    expect(tester.widget<WispServiceItem>(files).enabled, isTrue);
    await tester.tap(find.descendant(of: files, matching: find.text('files')));
    await tester.pumpAndSettle();
    expect(tester.widget<WispServiceItem>(files).enabled, isFalse);
    await tester.tap(find.byTooltip('Close').last);
    await tester.pumpAndSettle();
    expect(
      tester
          .widget<WispToggle>(
            find.byKey(const ValueKey('service-toggle-files')),
          )
          .value,
      isFalse,
    );
    expect(find.text('Off'), findsWidgets);
    await tester.tap(find.byTooltip('Settings'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Local services'));
    await tester.pumpAndSettle();
    await tester.tap(
      find.descendant(
        of: find.byType(ServiceSettings),
        matching: find.text('files'),
      ),
    );
    await tester.pumpAndSettle();
    await tester.tap(find.byTooltip('Close').last);
    await tester.pumpAndSettle();
    expect(find.byKey(const ValueKey('service-toggle-files')), findsOneWidget);
    expect(backend.status['host']['file_exports'], {
      'incoming': '/srv/receiving',
    });
    await close(tester);
  });
  testWidgets(
    'Android Local services opens the module list and can restore Files',
    (tester) async {
      final backend = AndroidLocalFake();
      await mount(tester, backend, size: const Size(600, 850));
      await tester.tap(find.byTooltip('Settings'));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Local services'));
      await tester.pumpAndSettle();
      expect(find.text('Local services'), findsNWidgets(2));
      expect(find.byType(ServiceSettings), findsOneWidget);
      final files = find.descendant(
        of: find.byType(ServiceSettings),
        matching: find.byWidgetPredicate(
          (w) => w is WispServiceItem && w.title == 'incoming',
        ),
      );
      final folder = find.descendant(
        of: files,
        matching: find.byIcon(Icons.folder_outlined),
      );
      final toggle = find.descendant(
        of: files,
        matching: find.byType(WispToggle),
      );
      final gear = find.descendant(
        of: files,
        matching: find.byTooltip('Folders and access for incoming'),
      );
      expect(
        tester.getRect(folder).right,
        lessThan(
          tester
              .getRect(
                find.descendant(of: files, matching: find.text('incoming')),
              )
              .left,
        ),
      );
      expect(
        tester.getRect(toggle).right,
        lessThanOrEqualTo(tester.getRect(gear).left),
      );
      await screenshot(tester, 'android-local-services');
      await tester.tap(
        find.descendant(of: files, matching: find.text('incoming')),
      );
      await tester.pumpAndSettle();
      expect(tester.widget<WispServiceItem>(files).enabled, isFalse);
      await tester.tap(find.byTooltip('Close').last);
      await tester.pumpAndSettle();
      expect(
        tester
            .widget<WispToggle>(
              find.byKey(const ValueKey('service-toggle-incoming')),
            )
            .value,
        isFalse,
      );
      expect(find.text('Off'), findsWidgets);
      await close(tester);
    },
  );
  testWidgets('local service device counts follow access changes', (
    tester,
  ) async {
    final backend = DesktopFake();
    await mount(tester, backend, size: const Size(360, 844));
    expect(find.text('1 device'), findsOneWidget);
    expect(find.text('0 devices'), findsOneWidget);
    backend.status['host']['services'][0]['allow'] = [
      peer,
      peer,
      'e' * 64,
      'd' * 64,
    ];
    await tester.pump(const Duration(seconds: 2));
    await tester.pumpAndSettle();
    expect(find.text('2 devices'), findsOneWidget);
    expect(find.text('1 device'), findsNothing);
    expect(tester.takeException(), isNull);
    await close(tester);
  });
  testWidgets(
    'public service links, status and copy action reflect publication state',
    (tester) async {
      final backend = DesktopFake();
      final publication = {
        'service': 'web',
        'incarnation': incarnation,
        'enabled': true,
        'state': 'online',
        'url': 'https://shared-web.wispnet.test/',
      };
      backend.status['public_access']['services'] = [publication];
      String? copied;
      tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
        SystemChannels.platform,
        (call) async {
          if (call.method == 'Clipboard.setData') {
            copied = call.arguments['text'] as String;
          }
          return null;
        },
      );
      addTearDown(
        () => tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
          SystemChannels.platform,
          null,
        ),
      );
      await mount(tester, backend, size: const Size(1000, 850));
      expect(find.text('Public'), findsOneWidget);
      expect(find.text('1 device'), findsNothing);
      expect(find.text('Public URL'), findsNothing);
      expect(find.byTooltip('Copy public URL for web'), findsNothing);
      final links = find.byTooltip('Show links for web');
      final gear = find.byTooltip('Settings for web on hibreak');
      expect(
        tester.getRect(links).right,
        lessThanOrEqualTo(tester.getRect(gear).left),
      );
      await tester.tap(links);
      await tester.pumpAndSettle();
      expect(copied, isNull);
      await tester.tap(find.byTooltip('Copy public URL'));
      await tester.pumpAndSettle();
      expect(copied, publication['url']);
      expect(backend.calls.where((call) => call.$1 == 'open'), isEmpty);
      await tester.tap(find.byTooltip('Close').last);
      await tester.pumpAndSettle();
      publication['state'] = 'offline';
      await tester.pump(const Duration(seconds: 2));
      await tester.pumpAndSettle();
      expect(find.text('Public'), findsOneWidget);
      await tester.tap(gear);
      await tester.pumpAndSettle();
      final local = find.text('http://shared-web.localhost:18000/');
      final public = find.text(publication['url'] as String);
      expect(tester.getRect(local).top, lessThan(tester.getRect(public).top));
      expect(
        tester.getRect(public).bottom,
        lessThan(tester.getRect(find.text('Edit')).top),
      );
      expect(find.textContaining('127.0.0.1:8080'), findsNothing);
      expect(
        find.descendant(
          of: find.byType(ServiceSettings),
          matching: find.textContaining('Public ·'),
        ),
        findsNothing,
      );
      await tester.tap(local);
      await tester.pumpAndSettle();
      expect(
        backend.calls.where((call) => call.$1 == 'launch').single.$2['url'],
        'http://shared-web.localhost:18000/',
      );
      await tester.tap(public);
      await tester.pumpAndSettle();
      expect(
        backend.calls
            .where((call) => call.$1 == 'launchPublic')
            .single
            .$2['service'],
        'web',
      );
      expect(backend.ownerCalls, isEmpty);
      await tester.tap(find.byTooltip('Close').last);
      await tester.pumpAndSettle();
      backend.status['public_access']['services'] = [];
      await tester.pump(const Duration(seconds: 2));
      await tester.pumpAndSettle();
      expect(find.text('Public'), findsNothing);
      expect(find.text('1 device'), findsOneWidget);
      expect(find.byTooltip('Copy public URL for web'), findsNothing);
      await close(tester);
    },
  );
  testWidgets('public status and copy controls fit a narrow service tile', (
    tester,
  ) async {
    final backend = DesktopFake();
    backend.status['public_access']['services'] = [
      {
        'service': 'web',
        'incarnation': incarnation,
        'enabled': true,
        'state': 'online',
        'url': 'https://shared-web.wispnet.test/',
      },
    ];
    await mount(tester, backend, size: const Size(360, 844));
    expect(find.text('Public'), findsOneWidget);
    expect(find.byTooltip('Copy public URL for web'), findsNothing);
    expect(find.byTooltip('Show links for web'), findsOneWidget);
    expect(tester.takeException(), isNull);
    final card = tester.widget<MachineCard>(find.byType(MachineCard).first);
    await tester.pumpWidget(
      MaterialApp(
        theme: wispTheme(),
        home: Scaffold(
          body: MediaQuery(
            data: const MediaQueryData(textScaler: TextScaler.linear(2)),
            child: SizedBox(width: 280, child: card),
          ),
        ),
      ),
    );
    await tester.pump();
    expect(find.text('Public URL'), findsNothing);
    expect(find.byTooltip('Show links for web'), findsOneWidget);
    expect(tester.takeException(), isNull);
    await close(tester);
  });
  testWidgets(
    'phone public links use matching host metadata and copy without opening a service',
    (tester) async {
      final publication = {
        'service': 'website',
        'incarnation': incarnation,
        'url': 'https://shared-website.wispnet.test/',
        'state': 'online',
      };
      final backend = FakeBackend(
        publicAccess: {
          'version': 1,
          'host': peer,
          'services': [publication],
        },
      );
      String? copied;
      tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
        SystemChannels.platform,
        (call) async {
          if (call.method == 'Clipboard.setData') {
            copied = call.arguments['text'] as String;
          }
          return null;
        },
      );
      addTearDown(
        () => tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
          SystemChannels.platform,
          null,
        ),
      );
      await mount(tester, backend, size: const Size(360, 844));
      expect(find.text('Public URL'), findsNothing);
      expect(find.text('Public · Online'), findsOneWidget);
      expect(find.byTooltip('Copy public URL for website'), findsNothing);
      final links = find.byWidgetPredicate(
        (widget) =>
            widget is IconButton && widget.tooltip == 'Show links for website',
      );
      expect(links, findsOneWidget);
      expect(tester.widget<IconButton>(links).onPressed, isNotNull);
      expect(
        find.descendant(of: links, matching: find.byIcon(Icons.link)),
        findsOneWidget,
      );
      await tester.tap(links);
      await tester.pumpAndSettle();
      expect(copied, isNull);
      expect(find.text('Links for website'), findsOneWidget);
      expect(find.text('Local URL'), findsOneWidget);
      expect(find.text('Wispnet URL'), findsOneWidget);
      expect(
        tester.getRect(find.text('Local URL')).top,
        lessThan(tester.getRect(find.text('Wispnet URL')).top),
      );
      expect(
        tester.getRect(find.text('Wispnet URL')).top,
        lessThan(tester.getRect(find.text('Public URL').last).top),
      );
      await tester.tap(find.byTooltip('Copy local URL'));
      await tester.pumpAndSettle();
      expect(copied, 'http://j-strix-aaaaaaaaaaaa-website.localhost:18400/');
      await tester.tap(find.byTooltip('Copy Wispnet link'));
      await tester.pumpAndSettle();
      expect(copied, 'wispnet://$peer/website');
      await tester.tap(find.byTooltip('Copy public URL'));
      await tester.pumpAndSettle();
      expect(copied, publication['url']);
      expect(
        backend.calls.where((c) => ['open', 'saveSettings'].contains(c.$1)),
        isEmpty,
      );
      expect(backend.calls.where((c) => c.$1 == 'launch'), isEmpty);
      await tester.tap(find.byTooltip('Close').last);
      await tester.pumpAndSettle();
      backend.publicAccess!['host'] = 'e' * 64;
      await tester.pump(const Duration(seconds: 2));
      await tester.pumpAndSettle();
      expect(find.text('Public · Online'), findsNothing);
      await tester.tap(find.byTooltip('Show links for website'));
      await tester.pumpAndSettle();
      expect(find.byTooltip('Copy public URL'), findsNothing);
      await tester.tap(find.byTooltip('Close').last);
      await tester.pumpAndSettle();
      backend.publicAccess!['host'] = peer;
      publication['incarnation'] = 'e' * 64;
      await tester.pump(const Duration(seconds: 2));
      await tester.pumpAndSettle();
      expect(find.text('Public · Online'), findsNothing);
      await tester.tap(find.byTooltip('Show links for website'));
      await tester.pumpAndSettle();
      expect(find.byTooltip('Copy public URL'), findsNothing);
      await tester.tap(find.byTooltip('Close').last);
      await tester.pumpAndSettle();
      await close(tester);
      publication['incarnation'] = incarnation;
      await mount(
        tester,
        FakeBackend(state: 'unreachable', publicAccess: backend.publicAccess),
      );
      expect(find.text('Public · Cached'), findsOneWidget);
      expect(find.text('Public · Online'), findsNothing);
      expect(find.text('Public URL'), findsNothing);
      await tester.tap(find.byTooltip('Show links for website'));
      await tester.pumpAndSettle();
      expect(find.byTooltip('Copy public URL'), findsOneWidget);
      await close(tester);
    },
  );
  testWidgets(
    'desktop link popup uses the same local, Wispnet and public options as mobile',
    (tester) async {
      final backend = DesktopFake();
      backend.status['public_access']['services'] = [
        {
          'service': 'web',
          'incarnation': incarnation,
          'enabled': true,
          'state': 'online',
          'url': 'https://shared-web.wispnet.test/',
        },
      ];
      await mount(tester, backend, size: const Size(1000, 850));
      await tester.tap(find.byTooltip('Show links for web'));
      await tester.pumpAndSettle();
      expect(find.text('http://shared-web.localhost:18000/'), findsOneWidget);
      expect(find.text('https://shared-web.wispnet.test/'), findsOneWidget);
      expect(find.byTooltip('Copy Wispnet link'), findsOneWidget);
      expect(find.text('wispnet://${'d' * 64}/web'), findsOneWidget);
      expect(
        tester.getRect(find.text('Local URL')).top,
        lessThan(tester.getRect(find.text('Wispnet URL')).top),
      );
      expect(
        tester.getRect(find.text('Wispnet URL')).top,
        lessThan(tester.getRect(find.text('Public URL').last).top),
      );
      expect(
        backend.calls.where((c) => ['open', 'launch'].contains(c.$1)),
        isEmpty,
      );
      await close(tester);
    },
  );
  testWidgets(
    'link popup retains portable addresses when local resolution fails and can retry',
    (tester) async {
      final backend =
          FakeBackend(
              publicAccess: {
                'version': 1,
                'host': peer,
                'services': [
                  {
                    'service': 'website',
                    'incarnation': incarnation,
                    'state': 'online',
                    'url': 'https://shared-web.wispnet.test/',
                  },
                ],
              },
            )
            ..missingRoute = true
            ..failOpen = true;
      await mount(tester, backend);
      await tester.tap(find.byTooltip('Show links for website'));
      await tester.pumpAndSettle();
      expect(find.text('Host unavailable'), findsOneWidget);
      expect(find.byTooltip('Copy Wispnet link'), findsOneWidget);
      expect(find.byTooltip('Copy public URL'), findsOneWidget);
      backend.failOpen = false;
      await tester.tap(find.text('Retry'));
      await tester.pumpAndSettle();
      expect(
        find.text('http://website.wispnet.localhost:18300'),
        findsOneWidget,
      );
      expect(
        backend.calls.where((c) => c.$1 == 'open').last.$2['incarnation'],
        incarnation,
      );
      expect(backend.calls.where((c) => c.$1 == 'launch'), isEmpty);
      await close(tester);
    },
  );
  testWidgets('desktop public access requires explicit confirmation', (
    tester,
  ) async {
    final backend = DesktopFake();
    await mount(tester, backend, size: const Size(1000, 850));
    await tester.tap(find.byTooltip('Settings for web on hibreak'));
    await tester.pumpAndSettle();
    await tester.ensureVisible(find.text('Enable public access'));
    await tester.tap(find.text('Enable public access'));
    await tester.pumpAndSettle();
    expect(backend.ownerCalls, isEmpty);
    expect(
      find.textContaining('unprotected to the public internet'),
      findsOneWidget,
    );
    expect(
      find.textContaining(
        'without pairing, device approval, or Wispnet authentication',
      ),
      findsOneWidget,
    );
    await tester.tap(find.text('Cancel'));
    await tester.pumpAndSettle();
    expect(backend.ownerCalls, isEmpty);
    await tester.tap(find.text('Enable public access'));
    await tester.pumpAndSettle();
    await tester.tap(find.widgetWithText(FilledButton, 'Enable public access'));
    await tester.pumpAndSettle();
    expect(backend.ownerCalls.single['op'], 'configure_public');
    expect(backend.ownerCalls.single['incarnation'], incarnation);
    expect(backend.ownerCalls.single['enabled'], true);
    await close(tester);
  });
  testWidgets(
    'local service danger actions have spacing and removal requires confirmation',
    (tester) async {
      final backend = DesktopFake();
      await mount(tester, backend, size: const Size(1000, 850));
      await tester.tap(find.byTooltip('Settings for web on hibreak'));
      await tester.pumpAndSettle();
      final edit = find.widgetWithText(OutlinedButton, 'Edit');
      final remove = find.widgetWithText(OutlinedButton, 'Remove service');
      final publish = find.widgetWithText(
        OutlinedButton,
        'Enable public access',
      );
      expect(
        tester.getRect(remove).top - tester.getRect(edit).bottom,
        greaterThanOrEqualTo(8),
      );
      expect(
        tester.getRect(publish).top - tester.getRect(remove).bottom,
        greaterThanOrEqualTo(8),
      );
      for (final button in [remove, publish]) {
        expect(
          tester
              .widget<OutlinedButton>(button)
              .style!
              .foregroundColor!
              .resolve({}),
          WispColors.error,
        );
      }
      await tester.tap(remove);
      await tester.pumpAndSettle();
      expect(backend.ownerCalls, isEmpty);
      expect(
        find.textContaining('Paired devices will lose Wispnet access'),
        findsOneWidget,
      );
      expect(
        find.textContaining(
          'does not stop or delete the underlying application',
        ),
        findsOneWidget,
      );
      await tester.tap(find.text('Cancel'));
      await tester.pumpAndSettle();
      expect(backend.ownerCalls, isEmpty);
      await tester.tap(remove);
      await tester.pumpAndSettle();
      await tester.tap(find.widgetWithText(FilledButton, 'Remove service'));
      await tester.pumpAndSettle();
      expect(backend.ownerCalls.single['op'], 'mutate');
      expect((backend.ownerCalls.single['operation'] as Map)['op'], 'remove');
      await close(tester);
    },
  );
  testWidgets('desktop pairing approval preserves existing service grants', (
    tester,
  ) async {
    final backend = DesktopFake();
    await mount(tester, backend, size: const Size(1000, 850));
    await tester.tap(
      find
          .descendant(
            of: find.byKey(const ValueKey('remote-sector')),
            matching: find.byType(OutlinedButton),
          )
          .first,
    );
    await tester.pumpAndSettle();
    await tester.tap(find.text('Review 1 pairing requests'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Approve device'));
    await tester.pumpAndSettle();
    final grant = tester.widget<WispToggleListTile>(
      find.widgetWithText(WispToggleListTile, 'web'),
    );
    expect(grant.value, true);
    expect(grant.onChanged, isNull);
    await tester.tap(find.text('Approve device').last);
    await tester.pumpAndSettle();
    expect(backend.ownerCalls.single['operation'], {
      'op': 'approve_invitation',
      'invitation': 'request-id',
      'services': ['web'],
      'expected_revision': 7,
    });
    await close(tester);
  });
  testWidgets(
    'Android shell opens the shared Flutter terminal with a native viewport',
    (tester) async {
      final backend = FakeBackend(shellService: true);
      await mount(tester, backend);
      await tester.tap(find.text('shell'));
      await tester.pumpAndSettle();
      expect(
        backend.calls.where((c) => c.$1 == 'nativeTerminalOpen').single.$2,
        {
          'host': peer,
          'host_name': 'j-strix',
          'service': 'shell',
          'incarnation': incarnation,
          'session': 'main',
        },
      );
      expect(find.byType(DesktopTerminalSheet), findsOneWidget);
      expect(find.byType(AndroidView), findsOneWidget);
      expect(find.byTooltip('Open in system terminal'), findsNothing);
      expect(
        backend.calls.where((c) => c.$1 == 'open' || c.$1 == 'launch'),
        isEmpty,
      );
      expect(find.byIcon(Icons.terminal), findsOneWidget);
      await close(tester);
    },
  );
  testWidgets(
    'Android shell dock expands and minimizes without a close button',
    (tester) async {
      final backend = FakeBackend(
        shellService: true,
        minimizedShell: {
          'host': peer,
          'host_name': 'j-strix',
          'service': 'shell',
          'incarnation': incarnation,
          'session': 'main',
          'title': 'j-strix · wispnet (2)',
          'state': 'attached',
          'tab_count': 2,
          'minimized': true,
        },
      );
      await mount(tester, backend);
      expect(find.byKey(const ValueKey('shell-dock')), findsOneWidget);
      expect(find.text('j-strix · main'), findsOneWidget);

      await tester.tap(find.byKey(const ValueKey('shell-dock-expand')));
      await tester.pumpAndSettle();
      expect(
        backend.calls.where((c) => c.$1 == 'nativeTerminalOpen').single.$2,
        {
          'host': peer,
          'host_name': 'j-strix',
          'service': 'shell',
          'incarnation': incarnation,
          'session': 'main',
        },
      );
      expect(find.byType(DesktopTerminalSheet), findsOneWidget);
      await tester.tap(find.byKey(const ValueKey('desktop-shell-close')));
      await tester.pumpAndSettle();

      expect(find.byKey(const ValueKey('shell-dock-close')), findsNothing);
      expect(
        backend.calls.where((c) => c.$1 == 'nativeTerminalClose'),
        isEmpty,
      );
      expect(find.byKey(const ValueKey('shell-dock')), findsOneWidget);
      await close(tester);
    },
  );
  testWidgets(
    'upload docks match shells and show only active receiving hosts',
    (tester) async {
      final secondHost = 'e' * 64;
      final backend = UploadDockFake()
        ..addUpload('first', peer, 'weights.bin')
        ..addUpload('second', secondHost, 'config.json')
        ..addUpload('old', peer, 'old.bin', state: 'complete');
      await mount(tester, backend);
      await tester.pump();
      final shell = find.byKey(const ValueKey('shell-dock'));
      final upload = find.byKey(const ValueKey('file-transfers-dock'));
      expect(tester.getSize(upload), tester.getSize(shell));
      expect(find.text('j-strix · Uploading file'), findsOneWidget);
      expect(find.text('j-desktop · Uploading file'), findsOneWidget);
      expect(find.text('Relay: connected'), findsNothing);
      expect(find.text('v0.1.0'), findsNothing);
      expect(find.textContaining('0 active'), findsNothing);
      await screenshot(tester, 'android-upload-shell-docks');

      await tester.tap(find.byKey(ValueKey('file-dock-expand-$peer')));
      await tester.pumpAndSettle();
      expect(find.text('Uploads to j-strix'), findsOneWidget);
      expect(find.text('weights.bin'), findsOneWidget);
      expect(find.text('config.json'), findsNothing);
      await screenshot(tester, 'android-upload-progress');
      await tester.tap(find.byTooltip('Close').last);
      await tester.pumpAndSettle();

      backend.uploads['first']!['state'] = 'complete';
      await tester.pump(const Duration(seconds: 1));
      await tester.pump();
      expect(find.text('j-strix · Uploading file'), findsNothing);
      expect(find.text('j-desktop · Uploading file'), findsOneWidget);
      backend.uploads['second']!['state'] = 'failed';
      await tester.pump(const Duration(seconds: 1));
      await tester.pump();
      expect(find.byKey(const ValueKey('file-transfers-dock')), findsNothing);
      expect(find.byType(WispDockBar), findsOneWidget);
      expect(shell, findsOneWidget);
      await close(tester);
    },
  );

  testWidgets(
    'closing an upload dock confirms cancellation for only its host',
    (tester) async {
      final secondHost = 'e' * 64;
      final backend = UploadDockFake()
        ..addUpload('first', peer, 'weights.bin')
        ..addUpload('second', peer, 'model.bin')
        ..addUpload('other', secondHost, 'config.json');
      await mount(tester, backend);
      await tester.pump();
      expect(find.text('j-strix · Uploading 2 files'), findsOneWidget);
      await tester.tap(find.byKey(ValueKey('file-dock-close-$peer')));
      await tester.pumpAndSettle();
      await tester.tap(find.text('Keep uploading'));
      await tester.pumpAndSettle();
      expect(backend.calls.where((call) => call.$1 == 'fileCancel'), isEmpty);
      await tester.tap(find.byKey(ValueKey('file-dock-close-$peer')));
      await tester.pumpAndSettle();
      await tester.tap(find.byKey(const ValueKey('confirm-cancel-uploads')));
      await tester.pumpAndSettle();
      await tester.pump(const Duration(seconds: 1));
      await tester.pump();
      expect(
        backend.calls
            .where((call) => call.$1 == 'fileCancel')
            .map((call) => call.$2['transfer']),
        unorderedEquals(['first', 'second']),
      );
      expect(backend.uploads['other']!['state'], 'running');
      expect(find.text('j-strix · Uploading 2 files'), findsNothing);
      expect(find.text('j-desktop · Uploading file'), findsOneWidget);
      await close(tester);
    },
  );

  testWidgets('finished upload history never creates an empty dock', (
    tester,
  ) async {
    final backend = UploadDockFake()
      ..addUpload('complete', peer, 'sent.bin', state: 'complete')
      ..addUpload('failed', peer, 'failed.bin', state: 'failed')
      ..addUpload('cancelled', peer, 'cancelled.bin', state: 'cancelled');
    await mount(tester, backend);
    await tester.pump();
    expect(find.byKey(const ValueKey('file-transfers-dock')), findsNothing);
    expect(find.textContaining('File transfers'), findsNothing);
    expect(find.byKey(const ValueKey('shell-dock')), findsOneWidget);
    await close(tester);
  });
  testWidgets(
    'multiple minimized host shells each render their own dock pill',
    (tester) async {
      final secondHost = 'e' * 64;
      final backend = FakeBackend(
        shellService: true,
        minimizedShells: [
          {
            'host': peer,
            'host_name': 'j-strix',
            'service': 'shell',
            'incarnation': incarnation,
            'session': 'main',
            'title': 'j-strix · fish (2)',
            'state': 'attached',
            'tab_count': 2,
            'minimized': true,
          },
          {
            'host': secondHost,
            'host_name': 'j-desktop',
            'service': 'shell',
            'incarnation': incarnation,
            'session': 'tab-2',
            'title': 'j-desktop · bash',
            'state': 'attached',
            'tab_count': 1,
            'minimized': true,
          },
        ],
      );
      await mount(tester, backend);
      expect(find.text('j-strix · main'), findsOneWidget);
      expect(find.text('j-desktop · tab-2'), findsOneWidget);

      expect(find.byKey(const ValueKey('shell-dock-close')), findsNothing);
      expect(
        find.byKey(ValueKey('shell-dock-close-$secondHost')),
        findsNothing,
      );
      await tester.tap(find.byKey(ValueKey('shell-dock-expand-$secondHost')));
      await tester.pumpAndSettle();
      expect(find.byType(DesktopTerminalSheet), findsOneWidget);
      await tester.tap(find.byKey(const ValueKey('desktop-shell-close')));
      await tester.pumpAndSettle();
      expect(
        backend.calls.where((c) => c.$1 == 'nativeTerminalClose'),
        isEmpty,
      );
      expect(find.text('j-strix · main'), findsOneWidget);
      expect(find.text('j-desktop · tab-2'), findsOneWidget);
      await close(tester);
    },
  );
  testWidgets(
    'desktop shell opens baked-in terminal sheet with tabs, splits, modes, external launch, and terminate on close',
    (tester) async {
      final backend = DesktopFake();
      await mount(tester, backend, size: const Size(1200, 900));
      await tester.tap(find.text('shell'));
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 200));

      expect(
        backend.calls.where((c) => c.$1 == 'shellOpen').single.$2['session'],
        'main',
      );
      expect(find.text(' · 9 ms'), findsOneWidget);
      expect(
        find.byKey(const ValueKey('desktop-shell-mode-button')),
        findsOneWidget,
      );

      // Open input mode popover and verify explanations
      await tester.tap(find.byKey(const ValueKey('desktop-shell-mode-button')));
      await tester.pump();
      expect(
        find.byKey(const ValueKey('desktop-shell-mode-popover')),
        findsOneWidget,
      );
      expect(
        find.textContaining('Shows typed characters immediately'),
        findsOneWidget,
      );
      await tester.tap(find.byKey(const ValueKey('desktop-shell-mode-sync')));
      await tester.pump();
      expect(
        find.byKey(const ValueKey('desktop-shell-mode-popover')),
        findsNothing,
      );

      // Split active tab side-by-side
      await tester.tap(
        find.byKey(const ValueKey('desktop-shell-split-vertical')),
      );
      await tester.pump();
      expect(backend.calls.where((c) => c.$1 == 'shellOpen'), hasLength(2));

      // Add a second tab on this host and close it explicitly (terminate: true)
      await tester.tap(find.byKey(const ValueKey('desktop-shell-new-tab')));
      await tester.pump();
      expect(backend.calls.where((c) => c.$1 == 'shellOpen'), hasLength(3));
      await tester.tap(
        find.byKey(const ValueKey('desktop-shell-close-tab-tab-3')),
      );
      await tester.pump();
      expect(backend.calls.where((c) => c.$1 == 'shellClose').single.$2, {
        'session': 'handle-tab-3',
        'terminate': true,
      });

      // Open in external system terminal
      await tester.tap(
        find.byKey(const ValueKey('desktop-shell-open-external')),
      );
      await tester.pump();
      expect(backend.calls.where((c) => c.$1 == 'openShell'), hasLength(1));

      // Popup × button minimizes to dock (does not close or terminate sessions)
      expect(
        find.descendant(
          of: find.byKey(const ValueKey('desktop-shell-close')),
          matching: find.byIcon(Icons.remove),
        ),
        findsOneWidget,
      );
      await tester.tap(find.byKey(const ValueKey('desktop-shell-close')));
      await tester.pumpAndSettle();
      expect(find.byKey(const ValueKey('shell-dock')), findsOneWidget);
      expect(backend.calls.where((c) => c.$1 == 'shellClose'), hasLength(1));

      // Clicking × on dock opens confirmation popup; confirming terminates remaining panes
      await tester.tap(find.byKey(const ValueKey('shell-dock-close')));
      await tester.pumpAndSettle();
      expect(find.text('Close terminal?'), findsOneWidget);
      await tester.tap(find.byKey(const ValueKey('confirm-close-shell')));
      await tester.pumpAndSettle();
      expect(find.byKey(const ValueKey('shell-dock')), findsNothing);
      final closeCalls = backend.calls
          .where((c) => c.$1 == 'shellClose')
          .toList();
      expect(closeCalls, hasLength(3));
      expect(closeCalls.every((c) => c.$2['terminate'] == true), isTrue);
      await close(tester);
    },
  );
  testWidgets(
    'desktop terminal close confirms termination and can be cancelled',
    (tester) async {
      final backend = DesktopFake();
      await mount(tester, backend, size: const Size(1200, 900));
      await tester.tap(find.text('shell'));
      await tester.pumpAndSettle();
      final closeButton = find.byKey(const ValueKey('desktop-shell-terminate'));
      expect(
        find.descendant(of: closeButton, matching: find.byIcon(Icons.close)),
        findsOneWidget,
      );
      await tester.tap(closeButton);
      await tester.pumpAndSettle();
      expect(find.text('Close terminal?'), findsOneWidget);
      expect(backend.calls.where((call) => call.$1 == 'shellClose'), isEmpty);
      await tester.tap(find.text('Cancel'));
      await tester.pumpAndSettle();
      expect(closeButton, findsOneWidget);
      await tester.tap(closeButton);
      await tester.pumpAndSettle();
      await tester.tap(
        find.byKey(const ValueKey('desktop-shell-confirm-close')),
      );
      await tester.pumpAndSettle();
      expect(closeButton, findsNothing);
      expect(find.byKey(const ValueKey('shell-dock')), findsNothing);
      expect(
        backend.calls
            .where((call) => call.$1 == 'shellClose')
            .single
            .$2['terminate'],
        isTrue,
      );
      await close(tester);
    },
  );
  test('terminal emulator buffer strips DCS/private CSI and answers fish startup queries', () {
    final replies = <String>[];
    final buffer = TerminalEmulatorBuffer(
      cols: 80,
      rows: 24,
      onReply: replies.add,
    );
    buffer.writeString(
      '\x1b[?u\x1b[>0q\x1b]11;?\x1b\\\x1b[?1049h'
      '\x1bP+q696e646e\x1b\\\x1bP+q71756572792d6f732d6e616d65\x1b\\'
      '\x1b[?1049l\x1b[0c'
      'Welcome to fish\r\n'
      '\x1b]0;johnny@j-strix: ~\x07'
      '\x1b[6n\x1b[?2004h\x1b[>4;1m\x1b[?u'
      'johnny@j-strix ~> ',
    );
    expect(buffer.lastTitle, 'johnny@j-strix: ~');
    expect(replies, contains('\x1b]11;rgb:0000/0000/0000\x1b\\'));
    expect(replies, contains('\x1b[?1;2c'));
    expect(replies, contains('\x1b[2;1R'));
    final line0 = buffer.lines[0].map((c) => c.char).join().trimRight();
    final line1 = buffer.lines[1].map((c) => c.char).join().trimRight();
    expect(line0, 'Welcome to fish');
    expect(line1, 'johnny@j-strix ~>');
  });
  testWidgets('popup sheets blur the background behind the modal barrier', (
    tester,
  ) async {
    final backend = FakeBackend();
    await mount(tester, backend);
    expect(find.byType(BackdropFilter), findsNothing);
    await tester.tap(find.byTooltip('Settings'));
    await tester.pumpAndSettle();
    expect(find.byType(BackdropFilter), findsOneWidget);
    await tester.tap(find.byTooltip('Close').last);
    await tester.pumpAndSettle();
    expect(find.byType(BackdropFilter), findsNothing);
    await close(tester);
  });
  testWidgets('built-in shell grants are managed without editing the service', (
    tester,
  ) async {
    final backend = DesktopFake();
    await mount(tester, backend, size: const Size(1000, 850));
    await tester.tap(find.byTooltip('Settings'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Local services'));
    await tester.pumpAndSettle();
    expect(find.text('Enable Shell'), findsNothing);
    await tester.tap(find.byTooltip('Shell access'));
    await tester.pumpAndSettle();
    expect(backend.ownerCalls, isEmpty);
    expect(find.widgetWithText(TextField, 'Port'), findsNothing);
    expect(find.byType(TextField).hitTestable(), findsNothing);
    expect(find.byType(DropdownButtonFormField<String>), findsNothing);
    expect(
      tester.widget<WispToggleListTile>(find.byType(WispToggleListTile)).value,
      false,
    );
    await tester.tap(find.byType(WispToggleListTile));
    await tester.ensureVisible(find.text('Save').last);
    await tester.tap(find.text('Save').last);
    await tester.pumpAndSettle();
    expect(backend.ownerCalls.single['operation'], {
      'op': 'shell',
      'allow': [peer],
      'expected_revision': 7,
    });
    await close(tester);
  });
  testWidgets('built-in shell settings show usage and actions before editing', (
    tester,
  ) async {
    final backend = DesktopFake();
    await mount(tester, backend, size: const Size(1000, 850));
    await tester.tap(find.byTooltip('Settings for shell on hibreak'));
    await tester.pumpAndSettle();
    expect(find.text('Manage access'), findsNothing);
    expect(find.text('Save'), findsNothing);
    expect(find.text('Remove service'), findsOneWidget);
    expect(find.text('Public access'), findsNothing);
    await tester.tap(find.text('Edit'));
    await tester.pumpAndSettle();
    expect(find.text('Save').last, findsOneWidget);
    // The overview stays mounted beneath the edit sheet.
    expect(find.text('Remove service'), findsOneWidget);
    expect(find.text('Edit'), findsOneWidget);
    expect(
      find.text('Built-in service · available on this computer'),
      findsNothing,
    );
    expect(find.byType(TextField).hitTestable(), findsNothing);
    await close(tester);
  });
  testWidgets('removing built-in shell disables it after confirmation', (
    tester,
  ) async {
    final backend = DesktopFake();
    await mount(tester, backend, size: const Size(1000, 850));
    await tester.tap(find.byTooltip('Settings for shell on hibreak'));
    await tester.pumpAndSettle();
    expect(find.text('wispnet shell ${'d' * 64}'), findsOneWidget);
    await tester.tap(find.widgetWithText(OutlinedButton, 'Remove service'));
    await tester.pumpAndSettle();
    expect(backend.ownerCalls, isEmpty);
    await tester.tap(find.widgetWithText(FilledButton, 'Remove service'));
    await tester.pumpAndSettle();
    expect(backend.ownerCalls.single['operation'], {
      'op': 'set_service_enabled',
      'id': 'shell',
      'incarnation': incarnation,
      'enabled': false,
      'expected_revision': 7,
    });
    await close(tester);
  });
  for (final android in [false, true]) {
    for (final inbox in [false, true]) {
      testWidgets(
        '${android ? 'Android' : 'desktop'} local ${inbox ? 'inbox' : 'share'} card uses the common open page',
        (tester) async {
          final backend = android ? AndroidLocalFake() : DesktopFake();
          final id = inbox ? 'incoming' : 'documents';
          if (!android || !inbox) {
            (backend.status['host']['services'] as List).add({
              'id': id,
              'kind': inbox ? 'file_inbox' : 'file_share',
              'incarnation': incarnation,
              'enabled': true,
              'allow': <String>[],
            });
          }
          await mount(
            tester,
            backend,
            size: android ? const Size(360, 780) : const Size(1000, 1000),
          );
          final card = find.descendant(
            of: find.byKey(const ValueKey('local-sector')),
            matching: find.text(id),
          );
          await tester.ensureVisible(card);
          await tester.tap(card);
          await tester.pumpAndSettle();
          expect(find.byType(ServiceSettings), findsNothing);
          if (inbox) {
            expect(find.text('Send files to hibreak'), findsOneWidget);
            expect(
              find.text(
                "wispnet files put ${'d' * 64} incoming /local/file filename",
              ),
              findsOneWidget,
            );
            expect(find.byType(SharedFilesBrowser), findsNothing);
          } else {
            expect(find.byType(SharedFilesBrowser), findsOneWidget);
            expect(find.text('documents on hibreak'), findsOneWidget);
            expect(
              backend.calls.where((call) => call.$1 == 'shareList').single.$2,
              {
                'host': 'd' * 64,
                'service': 'documents',
                'incarnation': incarnation,
                'path': '',
              },
            );
          }
          expect(
            backend.calls.where(
              (call) => call.$1 == 'open' || call.$1 == 'launch',
            ),
            isEmpty,
          );
          expect(backend.ownerCalls, isEmpty);
          expect(tester.takeException(), isNull);
          await close(tester);
        },
      );
    }
  }

  testWidgets('files card shows a send command without launching a browser', (
    tester,
  ) async {
    final backend = DesktopFake();
    (backend.status['host']['services'] as List).add({
      'id': 'files',
      'display_name': 'Files',
      'kind': 'files',
      'backend': '127.0.0.1:0',
      'incarnation': incarnation,
      'allow': <String>[],
    });
    await mount(tester, backend, size: const Size(1000, 1000));
    expect(find.byIcon(Icons.folder_outlined), findsOneWidget);
    await tester.tap(find.text('files'));
    await tester.pumpAndSettle();
    expect(find.text('Send files to hibreak'), findsOneWidget);
    expect(
      find.text("wispnet files put ${'d' * 64} '' /local/file filename"),
      findsOneWidget,
    );
    expect(
      find.text(
        'wispnet files get ${'d' * 64} WORKSPACE /local/file relative/path',
      ),
      findsNothing,
    );
    expect(
      backend.calls.where((call) => call.$1 == 'open' || call.$1 == 'launch'),
      isEmpty,
    );
    await close(tester);
  });
  testWidgets(
    'files sharing requires a folder before device grants can be selected',
    (tester) async {
      final backend = filesBackend();
      await mount(tester, backend, size: const Size(1000, 1000));
      await tester.tap(find.byTooltip('Settings for files on hibreak'));
      await tester.pumpAndSettle();
      expect(find.text('Remove service'), findsOneWidget);
      expect(find.text('Public access'), findsNothing);
      await tester.ensureVisible(find.text('Edit'));
      await tester.tap(find.text('Edit'));
      await tester.pumpAndSettle();
      expect(find.byType(TextField).hitTestable(), findsNothing);
      final checkbox = find.byType(WispToggleListTile);
      expect(tester.widget<WispToggleListTile>(checkbox).onChanged, isNull);
      expect(find.text('Permission'), findsOneWidget);
      expect(
        find.textContaining('without confirming each transfer'),
        findsOneWidget,
      );
      await tester.tap(find.text('Choose folder'));
      await tester.pumpAndSettle();
      expect(
        backend.calls.where((call) => call.$1 == 'pickFolder').single.$2,
        {},
      );
      expect(find.text('/srv/receiving'), findsOneWidget);
      expect(tester.widget<WispToggleListTile>(checkbox).onChanged, isNotNull);
      await tester.tap(checkbox);
      await tester.ensureVisible(find.text('Save').last);
      await tester.tap(find.text('Save').last);
      await tester.pumpAndSettle();
      expect(backend.ownerCalls.single['operation'], {
        'op': 'files',
        'allow': [peer],
        'exports': {'incoming': '/srv/receiving'},
        'expected_revision': 7,
      });
      await close(tester);
    },
  );
  testWidgets(
    'clearing the last shared folder disables devices and revokes Files grants',
    (tester) async {
      final backend = filesBackend(
        exports: {'training': '/srv/training'},
        allow: [peer],
      );
      await mount(tester, backend, size: const Size(1000, 1000));
      await tester.tap(find.byTooltip('Settings for files on hibreak'));
      await tester.pumpAndSettle();
      expect(find.text('Remove service'), findsOneWidget);
      expect(find.text('Public access'), findsNothing);
      await tester.ensureVisible(find.text('Edit'));
      await tester.tap(find.text('Edit'));
      await tester.pumpAndSettle();
      expect(
        tester
            .widget<WispToggleListTile>(find.byType(WispToggleListTile))
            .value,
        true,
      );
      await tester.tap(find.byTooltip('Remove receiving folder'));
      await tester.pump();
      final checkbox = tester.widget<WispToggleListTile>(
        find.byType(WispToggleListTile),
      );
      expect(checkbox.value, false);
      expect(checkbox.onChanged, isNull);
      await tester.ensureVisible(find.text('Save').last);
      await tester.tap(find.text('Save').last);
      await tester.pumpAndSettle();
      expect(backend.ownerCalls.single['operation'], {
        'op': 'files',
        'allow': [],
        'exports': {},
        'expected_revision': 7,
      });
      await close(tester);
    },
  );
  for (final configured in [false, true]) {
    testWidgets(
      'pairing ${configured ? 'permits' : 'disables'} Files grants ${configured ? 'with' : 'without'} a shared folder',
      (tester) async {
        final backend = filesBackend(
          exports: configured ? {'training': '/srv/training'} : {},
        );
        await mount(tester, backend, size: const Size(1000, 1000));
        await tester.tap(
          find
              .descendant(
                of: find.byKey(const ValueKey('remote-sector')),
                matching: find.byType(OutlinedButton),
              )
              .first,
        );
        await tester.pumpAndSettle();
        await tester.tap(find.text('Review 1 pairing requests'));
        await tester.pumpAndSettle();
        await tester.tap(find.text('Approve device'));
        await tester.pumpAndSettle();
        final grant = find.widgetWithText(WispToggleListTile, 'files');
        expect(tester.widget<WispToggleListTile>(grant).value, false);
        if (configured) {
          expect(tester.widget<WispToggleListTile>(grant).onChanged, isNotNull);
          await tester.tap(grant);
        } else {
          expect(tester.widget<WispToggleListTile>(grant).onChanged, isNull);
          expect(
            find.text('Set a shared folder before granting Files access.'),
            findsOneWidget,
          );
        }
        await tester.tap(find.text('Approve device').last);
        await tester.pumpAndSettle();
        expect(
          (backend.ownerCalls.single['operation'] as Map)['services'],
          configured ? ['web', 'files'] : ['web'],
        );
        await close(tester);
      },
    );
  }
  testWidgets('built-in files picks a receiving folder and explicit grants', (
    tester,
  ) async {
    final backend = DesktopFake();
    (backend.status['host']['services'] as List).add({
      'id': 'files',
      'display_name': 'Files',
      'kind': 'files',
      'backend': '127.0.0.1:0',
      'incarnation': incarnation,
      'allow': <String>[],
    });
    backend.status['host']['file_exports'] = {'training': '/srv/training'};
    await mount(tester, backend, size: const Size(1000, 1000));
    await tester.tap(find.byTooltip('Settings for files on hibreak'));
    await tester.pumpAndSettle();
    expect(find.text('Remove service'), findsOneWidget);
    expect(find.text('Public access'), findsNothing);
    await tester.ensureVisible(find.text('Edit'));
    await tester.tap(find.text('Edit'));
    await tester.pumpAndSettle();
    // The overview stays mounted beneath the edit sheet.
    expect(find.text('Remove service'), findsOneWidget);
    expect(find.text('Edit'), findsOneWidget);
    expect(find.byType(TextField).hitTestable(), findsNothing);
    expect(find.text('/srv/training'), findsOneWidget);
    expect(find.text('training'), findsNothing);
    backend.pickedFolder = '/srv/outputs';
    await tester.tap(find.byTooltip('Change receiving folder'));
    await tester.pumpAndSettle();
    expect(backend.calls.where((call) => call.$1 == 'pickFolder').single.$2, {
      'initialPath': '/srv/training',
    });
    expect(find.text('/srv/outputs'), findsOneWidget);
    expect(find.text('/srv/training'), findsNothing);
    await screenshot(tester, 'desktop-files-receiving-folder');
    await tester.tap(find.byType(WispToggleListTile));
    await tester.ensureVisible(find.text('Save').last);
    await tester.tap(find.text('Save').last);
    await tester.pumpAndSettle();
    expect(backend.ownerCalls.single['operation'], {
      'op': 'files',
      'allow': [peer],
      'exports': {'training': '/srv/outputs'},
      'expected_revision': 7,
    });
    await close(tester);
  });
  testWidgets(
    'cancelling or failing the receiving folder picker preserves settings',
    (tester) async {
      final backend = filesBackend(
        exports: {'incoming': '/srv/incoming'},
        allow: [peer],
      )..pickedFolder = null;
      await mount(tester, backend, size: const Size(1000, 1000));
      await tester.tap(find.byTooltip('Settings for files on hibreak'));
      await tester.pumpAndSettle();
      expect(find.text('Remove service'), findsOneWidget);
      expect(find.text('Public access'), findsNothing);
      await tester.ensureVisible(find.text('Edit'));
      await tester.tap(find.text('Edit'));
      await tester.pumpAndSettle();
      await tester.tap(find.byTooltip('Change receiving folder'));
      await tester.pumpAndSettle();
      expect(find.text('/srv/incoming'), findsOneWidget);
      expect(
        tester
            .widget<WispToggleListTile>(find.byType(WispToggleListTile))
            .value,
        true,
      );
      backend.folderPickerError = StateError('Folder picker unavailable');
      await tester.tap(find.byTooltip('Change receiving folder'));
      await tester.pumpAndSettle();
      expect(find.textContaining('Folder picker unavailable'), findsOneWidget);
      expect(find.text('/srv/incoming'), findsOneWidget);
      await tester.tap(find.text('Save').last);
      await tester.pumpAndSettle();
      expect(backend.ownerCalls.single['operation'], {
        'op': 'files',
        'allow': [peer],
        'exports': {'incoming': '/srv/incoming'},
        'expected_revision': 7,
      });
      await close(tester);
    },
  );
  testWidgets('a long receiving folder fits the narrow owner sheet', (
    tester,
  ) async {
    const path =
        '/home/johnny/Research data/very-long-receiving-folder-name/experiment-results/November 2026/received-files';
    final backend = filesBackend(exports: {'incoming': path}, allow: [peer]);
    await mount(tester, backend, size: const Size(390, 844));
    await tester.ensureVisible(find.byTooltip('Settings for files on hibreak'));
    await tester.tap(find.byTooltip('Settings for files on hibreak'));
    await tester.pumpAndSettle();
    expect(find.text('Remove service'), findsOneWidget);
    expect(find.text('Public access'), findsNothing);
    await tester.ensureVisible(find.text('Edit'));
    await tester.tap(find.text('Edit'));
    await tester.pumpAndSettle();
    expect(find.text(path), findsOneWidget);
    expect(find.byType(TextField).hitTestable(), findsNothing);
    final change = tester.getRect(find.byTooltip('Change receiving folder'));
    expect(change.right, lessThanOrEqualTo(390));
    expect(change.left, greaterThanOrEqualTo(0));
    expect(tester.takeException(), isNull);
    await screenshot(tester, 'desktop-files-receiving-folder-narrow');
    await close(tester);
  });
  testWidgets('grant-only saves preserve existing multiple receiving folders', (
    tester,
  ) async {
    final backend = filesBackend(
      exports: {'training': '/srv/training', 'outputs': '/srv/outputs'},
    );
    await mount(tester, backend, size: const Size(1000, 1000));
    await tester.tap(find.byTooltip('Settings for files on hibreak'));
    await tester.pumpAndSettle();
    expect(find.text('Remove service'), findsOneWidget);
    expect(find.text('Public access'), findsNothing);
    await tester.ensureVisible(find.text('Edit'));
    await tester.tap(find.text('Edit'));
    await tester.pumpAndSettle();
    expect(
      find.textContaining('replaces the 2 receiving folders'),
      findsOneWidget,
    );
    await tester.tap(find.byType(WispToggleListTile));
    await tester.tap(find.text('Save').last);
    await tester.pumpAndSettle();
    expect(backend.ownerCalls.single['operation'], {
      'op': 'files',
      'allow': [peer],
      'exports': {'training': '/srv/training', 'outputs': '/srv/outputs'},
      'expected_revision': 7,
    });
    expect(backend.calls.where((call) => call.$1 == 'pickFolder'), isEmpty);
    await close(tester);
  });
  testWidgets('ordinary services reject the reserved shell ID', (tester) async {
    final backend = DesktopFake();
    await mount(tester, backend, size: const Size(1000, 850));
    await tester.tap(find.byTooltip('Settings'));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Local services'));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const ValueKey('share-service')));
    await tester.pumpAndSettle();
    await tester.tap(find.text('Custom'));
    await tester.pumpAndSettle();
    await tester.enterText(
      find.widgetWithText(TextField, 'Service ID'),
      'shell',
    );
    await tester.ensureVisible(find.text('Save').last);
    await tester.tap(find.text('Save').last);
    await tester.pumpAndSettle();
    expect(
      find.text(
        '“shell” and “files” are reserved for built-in services. Use their access settings to manage permissions.',
      ),
      findsOneWidget,
    );
    expect(backend.ownerCalls, isEmpty);
    await tester.tap(find.byType(DropdownButtonFormField<String>));
    await tester.pumpAndSettle();
    expect(
      find.widgetWithText(DropdownMenuItem<String>, 'Shell'),
      findsNothing,
    );
    await close(tester);
  });
  test('shell remains unavailable when owner approval is pending', () {
    final shell = {'kind': 'shell', 'incarnation': incarnation};
    expect(Machine(peer, {'state': 'available'}).canOpen(shell), true);
    expect(Machine(peer, {'state': 'approval_pending'}).canOpen(shell), false);
    expect(Machine(peer, {'state': 'host_denied'}).canOpen(shell), false);
    expect(
      Machine(peer, {
        'state': 'available',
      }).canOpen({'kind': 'tcp', 'incarnation': incarnation}),
      false,
    );
  });
  test('saved order ignores missing and duplicate entries', () {
    expect(orderedIds(['a', 'b', 'c'], ['b', 'gone', 'b']), ['b', 'a', 'c']);
  });
}
