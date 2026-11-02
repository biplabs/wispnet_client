import 'dart:async';
import 'dart:io';
import 'dart:ui' as ui;

import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:wispnet/src/backend.dart';
import 'package:wispnet/src/app.dart';
import 'package:wispnet/src/service_settings.dart';
import 'package:wispnet/src/file_shares.dart';
import 'package:wispnet/ui.dart';

final host = 'a' * 64;
final incarnation = 'b' * 64;
final service = {
  'id': 'checkpoints',
  'kind': 'file_share',
  'incarnation': incarnation,
};
final machine = Machine(host, {'name': 'Training host', 'state': 'available'});

class ShareBackend implements WispnetBackend {
  final calls = <(String, Map<String, dynamic>)>[];
  String? selectedFolder = '/chosen';
  Map<String, dynamic> status = {
    'transfer': 'c' * 64,
    'state': 'running',
    'phase': 'downloading',
    'verified_bytes': 128,
    'size_bytes': 1024,
  };
  bool deny = false;
  Object? statusError;
  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    calls.add((method, Map.of(arguments)));
    switch (method) {
      case 'snapshot':
        return {
          'desktop': true,
          'enabled': true,
          'peer': 'f' * 64,
          'name': 'Laptop',
          'network': 'Network available',
          'order': {},
          'hosts': {
            host: {
              ...machine.data,
              'catalog': {
                'host': host,
                'services': [service],
              },
            },
          },
        };
      case 'refreshHost':
      case 'takeLink':
      case 'takeInvitation':
        return {};
      case 'ownerStatus':
        return {
          'host': {
            'host': 'f' * 64,
            'revision': 7,
            'services': [],
            'clients': [
              {'peer': host, 'name': 'Training host'},
            ],
            'file_shares': {},
          },
          'invitations': [],
        };
      case 'ownerData':
        return {'value': []};
      case 'ownerRequest':
        return {};
      case 'shareList':
        if (deny) throw const FormatException('PERMISSION_DENIED');
        if (arguments['path'] == 'run-1') {
          return {
            'entries': [
              {
                'name': 'weights.safetensors',
                'path': 'run-1/weights.safetensors',
                'is_dir': false,
                'size_bytes': 1024,
              },
            ],
          };
        }
        if (arguments['cursor'] != null) {
          return {
            'entries': [
              {
                'name': 'later.bin',
                'path': 'later.bin',
                'is_dir': false,
                'size_bytes': 64,
              },
            ],
          };
        }
        return {
          'entries': [
            {'name': 'run-1', 'path': 'run-1', 'is_dir': true, 'size_bytes': 0},
            {
              'name': 'checkpoint.bin',
              'path': 'checkpoint.bin',
              'is_dir': false,
              'size_bytes': 1024,
            },
          ],
          'next_cursor': 'page-2',
        };
      case 'pickFolder':
      case 'pickDownloadFolder':
        return selectedFolder == null
            ? {}
            : {'path': selectedFolder, 'label': 'Downloads'};
      case 'shareGet':
        return Map.of(status);
      case 'fileStatus':
        if (statusError case final Object error) throw error;
        return Map.of(status);
      case 'fileCancel':
        status = {...status, 'state': 'cancelled'};
        return Map.of(status);
      default:
        throw StateError('Unexpected method $method');
    }
  }
}

class LongShareBackend extends ShareBackend {
  final startReply = Completer<Map<String, dynamic>>();
  final lastFolder = 'f' * 40;

  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    if (method == 'shareList') {
      calls.add((method, Map.of(arguments)));
      return {
        'entries': [
          for (var index = 0; index < 24; index++)
            {
              'name': index == 23 ? lastFolder : 'checkpoint-$index',
              'path': index == 23 ? lastFolder : 'checkpoint-$index',
              'is_dir': true,
              'size_bytes': 0,
            },
        ],
      };
    }
    if (method == 'shareGet') {
      calls.add((method, Map.of(arguments)));
      return startReply.future;
    }
    return super.invoke(method, arguments);
  }
}

class DelayedStatusShareBackend extends ShareBackend {
  final statusReply = Completer<Map<String, dynamic>>();
  final cancelReply = Completer<Map<String, dynamic>>();
  bool holdStatus = false;
  bool holdCancel = false;

  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    if (method == 'fileStatus' && holdStatus) {
      calls.add((method, Map.of(arguments)));
      return statusReply.future;
    }
    if (method == 'fileCancel' && holdCancel) {
      calls.add((method, Map.of(arguments)));
      return cancelReply.future;
    }
    return super.invoke(method, arguments);
  }
}

Map<String, dynamic> downloadRequest() => {
  'host': host,
  'service': 'checkpoints',
  'incarnation': incarnation,
  'path': 'run-1',
  'local_path': '/chosen/run-1',
  'name': 'run-1',
  'recursive': true,
};

Map<String, dynamic> downloadStatus({
  required int verified,
  int? downloaded,
  int total = 10000,
  String phase = 'transferring',
  String state = 'running',
  String? transfer,
  int filesComplete = 0,
}) => {
  'transfer': transfer ?? 'c' * 64,
  'state': state,
  'phase': phase,
  'verified_bytes': verified,
  'downloaded_bytes': ?downloaded,
  'size_bytes': total,
  'files_complete': filesComplete,
  'files_total': 10,
};

Future<void> browser(
  WidgetTester tester,
  ShareBackend backend,
  SharedFileDownloads downloads, {
  bool desktop = true,
}) async {
  await tester.pumpWidget(
    MaterialApp(
      theme: wispTheme(),
      home: Scaffold(
        body: SingleChildScrollView(
          child: SharedFilesBrowser(
            backend: backend,
            machine: machine,
            service: service,
            downloads: downloads,
            desktop: desktop,
          ),
        ),
      ),
    ),
  );
  await tester.pump();
}

Future<void> captureShare(WidgetTester tester, String name) async {
  final folder = Platform.environment['WISPNET_SHARE_SCREENSHOTS'];
  if (folder == null) return;
  await tester.runAsync(() async {
    final boundary = tester.firstRenderObject<RenderRepaintBoundary>(
      find.byKey(const ValueKey('share-preview')),
    );
    final image = await boundary.toImage();
    final bytes = await image.toByteData(format: ui.ImageByteFormat.png);
    Directory(folder).createSync(recursive: true);
    File('$folder/$name.png').writeAsBytesSync(bytes!.buffer.asUint8List());
    image.dispose();
  });
}

void main() {
  setUpAll(() async {
    final fonts = FontLoader('Roboto')
      ..addFont(rootBundle.load('assets/fonts/Roboto-Regular.ttf'))
      ..addFont(rootBundle.load('assets/fonts/Roboto-Medium.ttf'))
      ..addFont(rootBundle.load('assets/fonts/Roboto-Bold.ttf'));
    await fonts.load();
    await (FontLoader(
      'MaterialIcons',
    )..addFont(rootBundle.load('fonts/MaterialIcons-Regular.otf'))).load();
  });
  test('read-only shares can open without an inbox service', () {
    expect(machine.canOpen(service), isTrue);
    expect(machine.canOpen({...service, 'enabled': false}), isFalse);
  });

  test('download speed measures fresh payload and expires during a stall', () {
    var elapsed = const Duration(days: 1);
    final job = SharedFileDownload(
      request: downloadRequest(),
      name: 'run-1',
      elapsed: () => elapsed,
    );
    job.updateStatus(downloadStatus(verified: 4000, downloaded: 0));
    expect(
      job.bytesPerSecond,
      isNull,
      reason: 'A resumed prefix is a baseline, not newly downloaded data.',
    );
    expect(job.estimatedRemaining, isNull);
    elapsed += const Duration(seconds: 2);
    job.updateStatus(downloadStatus(verified: 5000, downloaded: 1000));
    expect(job.bytesPerSecond, 500);
    expect(job.estimatedRemaining, const Duration(seconds: 10));
    // A completed/skipped child can make verified progress jump without
    // carrying that many new bytes over the network.
    elapsed += const Duration(seconds: 2);
    job.updateStatus(
      downloadStatus(verified: 9000, downloaded: 2000, filesComplete: 4),
    );
    expect(job.bytesPerSecond, 500);
    expect(job.estimatedRemaining, const Duration(seconds: 2));
    job.updateStatus(
      downloadStatus(verified: 9000, downloaded: 2000, filesComplete: 4),
    );
    expect(
      job.bytesPerSecond,
      500,
      reason: 'Repeated observations at the same time cannot divide by zero.',
    );
    elapsed += const Duration(seconds: 1);
    job.updateStatus(
      downloadStatus(verified: 9000, downloaded: 2000, filesComplete: 4),
    );
    expect(
      job.bytesPerSecond,
      400,
      reason:
          'An unchanged poll smooths chunk bursts instead of flashing zero.',
    );
    expect(job.estimatedRemaining, isNotNull);
    elapsed += const Duration(seconds: 1);
    job.updateStatus(
      downloadStatus(verified: 9500, downloaded: 2500, filesComplete: 4),
    );
    expect(job.bytesPerSecond, closeTo(2500 / 6, .001));
    elapsed += const Duration(seconds: 3);
    expect(
      job.bytesPerSecond,
      0,
      reason: 'An unanswered native poll must expire the old moving rate.',
    );
    expect(job.estimatedRemaining, isNull);
  });

  test(
    'download estimates reset across phases, handles and counter rollback',
    () {
      var elapsed = Duration.zero;
      var bytes = 1000;
      final job = SharedFileDownload(
        request: downloadRequest(),
        name: 'run-1',
        elapsed: () => elapsed,
      );
      void report({String phase = 'transferring', String state = 'running'}) {
        elapsed += const Duration(seconds: 1);
        job.updateStatus(
          downloadStatus(
            verified: bytes,
            downloaded: bytes,
            total: 100000,
            phase: phase,
            state: state,
          ),
        );
      }

      report();
      for (final phase in ['preparing', 'queued', 'verifying', 'saving']) {
        bytes += 1000;
        report();
        expect(job.bytesPerSecond, 1000);
        report(phase: phase);
        expect(job.bytesPerSecond, isNull, reason: phase);
        expect(job.estimatedRemaining, isNull, reason: phase);
        bytes += 10000;
        report();
        expect(
          job.bytesPerSecond,
          isNull,
          reason: 'Preparation and restored prefixes need a new baseline.',
        );
      }
      for (final state in ['cancelled', 'failed', 'complete']) {
        bytes += 1000;
        report();
        expect(job.bytesPerSecond, 1000);
        report(state: state);
        expect(job.bytesPerSecond, isNull, reason: state);
        expect(job.estimatedRemaining, isNull, reason: state);
        report();
        expect(job.bytesPerSecond, isNull);
      }
      elapsed += const Duration(seconds: 1);
      job.updateStatus(downloadStatus(verified: 2000, downloaded: 0));
      expect(
        job.bytesPerSecond,
        isNull,
        reason: 'Counter rollback cannot become a negative rate.',
      );
      elapsed += const Duration(seconds: 1);
      job.updateStatus(downloadStatus(verified: 3000, downloaded: 1000));
      expect(job.bytesPerSecond, 1000);
      elapsed += const Duration(seconds: 1);
      job.updateStatus(
        downloadStatus(verified: 8000, downloaded: 6000, transfer: 'd' * 64),
      );
      expect(
        job.bytesPerSecond,
        isNull,
        reason: 'A new native handle starts a new attempt.',
      );
      elapsed -= const Duration(seconds: 5);
      job.updateStatus(
        downloadStatus(verified: 9000, downloaded: 7000, transfer: 'd' * 64),
      );
      expect(
        job.bytesPerSecond,
        isNull,
        reason: 'A clock discontinuity cannot produce an invalid speed.',
      );
    },
  );

  testWidgets(
    'legacy folder work counters cannot masquerade as network speed',
    (tester) async {
      var elapsed = Duration.zero;
      final downloads = SharedFileDownloads(ShareBackend());
      final job = SharedFileDownload(
        request: downloadRequest(),
        name: 'run-1',
        elapsed: () => elapsed,
      );
      // The same completed-file count can describe the end of one child and
      // a retained prefix in the next. No fresh bytes need cross the network.
      job.updateStatus(downloadStatus(verified: 1000, filesComplete: 1));
      elapsed += const Duration(seconds: 1);
      job.updateStatus(downloadStatus(verified: 9000, filesComplete: 1));
      expect(job.canMeasureSpeed, isFalse);
      expect(job.bytesPerSecond, isNull);
      expect(job.estimatedRemaining, isNull);
      downloads.jobs.add(job);
      await tester.pumpWidget(
        MaterialApp(
          theme: wispTheme(),
          home: Scaffold(body: SharedDownloadsList(downloads: downloads)),
        ),
      );
      expect(find.text('Download speed unavailable'), findsOneWidget);
      expect(find.text('Measuring download speed…'), findsNothing);
      expect(find.textContaining('remaining'), findsNothing);
      expect(tester.takeException(), isNull);
      await tester.pumpWidget(const SizedBox());
      downloads.dispose();
    },
  );

  test(
    'legacy single-file progress keeps a useful rate without unknown ETAs',
    () {
      var elapsed = Duration.zero;
      final job = SharedFileDownload(
        request: {...downloadRequest(), 'recursive': false},
        name: 'model.bin',
        elapsed: () => elapsed,
      );
      job.updateStatus(downloadStatus(verified: 1000));
      expect(job.canMeasureSpeed, isTrue);
      expect(job.bytesPerSecond, isNull);
      elapsed += const Duration(seconds: 1);
      job.updateStatus(downloadStatus(verified: 2000));
      expect(job.bytesPerSecond, 1000);
      expect(job.estimatedRemaining, const Duration(seconds: 8));
      elapsed += const Duration(seconds: 1);
      job.updateStatus(downloadStatus(verified: 3000, total: 0));
      expect(
        job.estimatedRemaining,
        isNull,
        reason: 'Unknown totals cannot yield a remaining-time estimate.',
      );
      elapsed += const Duration(seconds: 1);
      job.updateStatus(downloadStatus(verified: 10000));
      expect(
        job.estimatedRemaining,
        isNull,
        reason: 'Verification may still be running after all bytes arrive.',
      );
    },
  );

  test('a long sampling gap establishes a fresh payload baseline', () {
    var elapsed = Duration.zero;
    final job = SharedFileDownload(
      request: downloadRequest(),
      name: 'run-1',
      elapsed: () => elapsed,
    );
    job.updateStatus(downloadStatus(verified: 1000, downloaded: 0));
    elapsed += const Duration(seconds: 1);
    job.updateStatus(downloadStatus(verified: 2000, downloaded: 1000));
    expect(job.bytesPerSecond, 1000);
    elapsed += const Duration(seconds: 19);
    job.updateStatus(downloadStatus(verified: 6000, downloaded: 5000));
    expect(
      job.bytesPerSecond,
      isNull,
      reason: 'A delayed native reply is not a recent speed measurement.',
    );
    expect(job.estimatedRemaining, isNull);
    elapsed += const Duration(seconds: 1);
    job.updateStatus(downloadStatus(verified: 7000, downloaded: 6000));
    expect(job.bytesPerSecond, 1000);
    expect(job.estimatedRemaining, const Duration(seconds: 3));
  });

  test(
    'download rate leaves an old burst out of its recent sampling window',
    () {
      var elapsed = Duration.zero;
      final job = SharedFileDownload(
        request: downloadRequest(),
        name: 'run-1',
        elapsed: () => elapsed,
      );
      job.updateStatus(
        downloadStatus(verified: 0, downloaded: 0, total: 100000),
      );
      for (var second = 1; second <= 12; second++) {
        elapsed = Duration(seconds: second);
        final bytes = 10000 + (second - 1) * 1000;
        job.updateStatus(
          downloadStatus(verified: bytes, downloaded: bytes, total: 100000),
        );
      }
      expect(
        job.bytesPerSecond,
        1000,
        reason: 'The startup burst must not inflate speed for the whole job.',
      );
    },
  );

  test(
    'restored and resumed downloads establish fresh rate baselines',
    () async {
      var elapsed = Duration.zero;
      final backend = ShareBackend();
      final downloads = SharedFileDownloads(backend, elapsed: () => elapsed);
      addTearDown(downloads.dispose);
      downloads.restore([
        {
          ...downloadStatus(verified: 8000, downloaded: 6000),
          'name': 'run-1',
          'request': downloadRequest(),
        },
      ]);
      final job = downloads.jobs.single;
      expect(job.bytesPerSecond, isNull);
      elapsed += const Duration(seconds: 1);
      backend.status = downloadStatus(verified: 9000, downloaded: 7000);
      await downloads.poll();
      expect(job.bytesPerSecond, 1000);
      expect(job.estimatedRemaining, const Duration(seconds: 1));
      await downloads.cancel(job);
      expect(job.bytesPerSecond, isNull);
      expect(job.estimatedRemaining, isNull);
      backend.status = downloadStatus(
        verified: 9500,
        downloaded: 0,
        transfer: 'd' * 64,
      );
      await downloads.start(job);
      expect(
        job.bytesPerSecond,
        isNull,
        reason: 'Resume must not count the retained destination bytes.',
      );
      elapsed += const Duration(seconds: 1);
      backend.status = downloadStatus(
        verified: 9600,
        downloaded: 100,
        transfer: 'd' * 64,
      );
      await downloads.poll();
      expect(job.bytesPerSecond, 100);
      expect(job.estimatedRemaining, const Duration(seconds: 4));
    },
  );

  test(
    'a failed owner poll clears rate and requires a fresh observation',
    () async {
      var elapsed = Duration.zero;
      final backend = ShareBackend()
        ..status = downloadStatus(verified: 1000, downloaded: 0);
      final downloads = SharedFileDownloads(backend, elapsed: () => elapsed);
      addTearDown(downloads.dispose);
      final job = SharedFileDownload(
        request: downloadRequest(),
        name: 'run-1',
        elapsed: () => elapsed,
      );
      await downloads.start(job);
      elapsed += const Duration(seconds: 1);
      backend.status = downloadStatus(verified: 2000, downloaded: 1000);
      await downloads.poll();
      expect(job.bytesPerSecond, 1000);
      backend.statusError = const FormatException('Local device unavailable');
      elapsed += const Duration(seconds: 1);
      await downloads.poll();
      expect(job.bytesPerSecond, isNull);
      expect(job.estimatedRemaining, isNull);
      backend.statusError = null;
      backend.status = downloadStatus(verified: 8000, downloaded: 7000);
      elapsed += const Duration(seconds: 1);
      await downloads.poll();
      expect(
        job.bytesPerSecond,
        isNull,
        reason: 'The disconnected interval must not become a speed sample.',
      );
    },
  );

  test(
    'pausing hides estimates before the native acknowledgment arrives',
    () async {
      var elapsed = Duration.zero;
      final backend = DelayedStatusShareBackend()
        ..status = downloadStatus(verified: 1000, downloaded: 0)
        ..holdCancel = true;
      final downloads = SharedFileDownloads(backend, elapsed: () => elapsed);
      addTearDown(downloads.dispose);
      final job = SharedFileDownload(
        request: downloadRequest(),
        name: 'run-1',
        elapsed: () => elapsed,
      );
      await downloads.start(job);
      elapsed += const Duration(seconds: 1);
      backend.status = downloadStatus(verified: 2000, downloaded: 1000);
      await downloads.poll();
      expect(job.bytesPerSecond, 1000);
      final cancelling = downloads.cancel(job);
      expect(backend.cancelReply.isCompleted, isFalse);
      expect(job.status['state'], 'running');
      expect(job.cancelling, isTrue);
      expect(job.bytesPerSecond, isNull);
      expect(job.estimatedRemaining, isNull);
      backend.cancelReply.complete({...backend.status, 'state': 'cancelled'});
      await cancelling;
      expect(job.cancelling, isFalse);
      expect(job.status['state'], 'cancelled');
    },
  );

  for (final size in [const Size(960, 900), const Size(360, 780)]) {
    testWidgets(
      'download speed and ETA remain current at ${size.width.toInt()}px while a native poll waits',
      (tester) async {
        tester.view.devicePixelRatio = 1;
        tester.view.physicalSize = size;
        addTearDown(tester.view.resetPhysicalSize);
        addTearDown(tester.view.resetDevicePixelRatio);
        var elapsed = Duration.zero;
        final backend = DelayedStatusShareBackend()
          ..status = downloadStatus(
            verified: 1024,
            downloaded: 0,
            total: 10240,
          );
        final downloads = SharedFileDownloads(backend, elapsed: () => elapsed);
        addTearDown(downloads.dispose);
        final job = SharedFileDownload(
          request: downloadRequest(),
          name: 'f' * 40,
          elapsed: () => elapsed,
        );
        await downloads.start(job);
        await tester.pumpWidget(
          RepaintBoundary(
            key: const ValueKey('share-preview'),
            child: MaterialApp(
              debugShowCheckedModeBanner: false,
              theme: wispTheme(),
              home: Scaffold(
                body: SingleChildScrollView(
                  padding: const EdgeInsets.all(20),
                  child: SharedDownloadsList(downloads: downloads),
                ),
              ),
            ),
          ),
        );
        expect(
          find.text('Measuring download speed…').hitTestable(),
          findsOneWidget,
        );
        elapsed += const Duration(seconds: 1);
        backend.status = downloadStatus(
          verified: 2048,
          downloaded: 1024,
          total: 10240,
        );
        await downloads.poll();
        await tester.pump();
        expect(find.text('1.0 KB/s').hitTestable(), findsOneWidget);
        expect(find.text('About 8s remaining').hitTestable(), findsOneWidget);
        await captureShare(
          tester,
          '${size.width < 500 ? 'phone' : 'desktop'}-download-rate',
        );

        backend.holdStatus = true;
        final pending = downloads.poll();
        elapsed += const Duration(seconds: 3);
        await tester.pump(const Duration(seconds: 3));
        expect(backend.statusReply.isCompleted, isFalse);
        expect(find.text('0 B/s').hitTestable(), findsOneWidget);
        expect(
          find.text('Estimating time remaining…').hitTestable(),
          findsOneWidget,
        );
        expect(
          find.text('About 8s remaining'),
          findsNothing,
          reason: 'A blocked owner call must not leave an old ETA onscreen.',
        );
        backend.statusReply.complete({...backend.status, 'state': 'complete'});
        await pending;
        await tester.pump();
        expect(find.text('0 B/s'), findsNothing);
        expect(find.text('Estimating time remaining…'), findsNothing);
        expect(tester.takeException(), isNull);
        await tester.pumpWidget(const SizedBox());
      },
    );
  }

  testWidgets('remaining time rounds up in seconds, minutes and hours', (
    tester,
  ) async {
    for (final (remainingBytes, label) in [
      (500, 'About 1s remaining'),
      (8000, 'About 8s remaining'),
      (61000, 'About 2m remaining'),
      (3601000, 'About 2h remaining'),
    ]) {
      var elapsed = Duration.zero;
      final downloads = SharedFileDownloads(ShareBackend());
      final job = SharedFileDownload(
        request: downloadRequest(),
        name: 'run-1',
        elapsed: () => elapsed,
      );
      job.updateStatus(
        downloadStatus(
          verified: 1000,
          downloaded: 0,
          total: 2000 + remainingBytes,
        ),
      );
      elapsed += const Duration(seconds: 1);
      job.updateStatus(
        downloadStatus(
          verified: 2000,
          downloaded: 1000,
          total: 2000 + remainingBytes,
        ),
      );
      downloads.jobs.add(job);
      await tester.pumpWidget(
        MaterialApp(
          theme: wispTheme(),
          home: Scaffold(body: SharedDownloadsList(downloads: downloads)),
        ),
      );
      expect(find.text(label), findsOneWidget);
      expect(tester.takeException(), isNull);
      await tester.pumpWidget(const SizedBox());
      downloads.dispose();
    }
  });

  testWidgets(
    'browser navigates folders and paginates within its named share',
    (tester) async {
      final backend = ShareBackend();
      final downloads = SharedFileDownloads(backend);
      addTearDown(downloads.dispose);
      await browser(tester, backend, downloads);
      expect(backend.calls.first.$1, 'shareList');
      expect(backend.calls.first.$2, {
        'host': host,
        'service': 'checkpoints',
        'incarnation': incarnation,
        'path': '',
      });
      await tester.tap(find.text('Load more files'));
      await tester.pump();
      expect(find.text('later.bin'), findsOneWidget);
      expect(backend.calls.last.$2['cursor'], 'page-2');
      await tester.tap(find.text('run-1'));
      await tester.pump();
      expect(find.text('weights.safetensors'), findsOneWidget);
      expect(find.text('later.bin'), findsNothing);
      expect(backend.calls.last.$2['path'], 'run-1');
      await tester.tap(find.byTooltip('Parent folder'));
      await tester.pump();
      expect(backend.calls.last.$2['path'], '');
    },
  );

  testWidgets(
    'remote share opens in the app and its download remains reachable from the dock',
    (tester) async {
      final backend = ShareBackend();
      tester.platformDispatcher.accessibilityFeaturesTestValue =
          const FakeAccessibilityFeatures(disableAnimations: true);
      addTearDown(
        tester.platformDispatcher.clearAccessibilityFeaturesTestValue,
      );
      await tester.pumpWidget(
        MediaQuery(
          data: const MediaQueryData(
            size: Size(800, 600),
            disableAnimations: true,
          ),
          child: WispnetApp(backend: backend),
        ),
      );
      await tester.pumpAndSettle();
      await tester.tap(find.text('checkpoints'));
      await tester.pumpAndSettle();
      expect(
        find.text(
          'Read-only share. Choose files or folders to download to this device. Inbox access is separate.',
        ),
        findsOneWidget,
      );
      await tester.ensureVisible(find.byTooltip('Download checkpoint.bin'));
      await tester.tap(find.byTooltip('Download checkpoint.bin'));
      await tester.pumpAndSettle();
      await tester.ensureVisible(find.byTooltip('Close'));
      await tester.tap(find.byTooltip('Close'));
      await tester.pumpAndSettle();
      expect(
        find.byKey(const ValueKey('share-downloads-dock')),
        findsOneWidget,
      );
      await tester.tap(find.byKey(const ValueKey('share-downloads-expand')));
      await tester.pumpAndSettle();
      expect(find.text('Cancel download'), findsOneWidget);
      await tester.tap(find.text('Cancel download'));
      await tester.pumpAndSettle();
      expect(backend.calls.where((c) => c.$1 == 'fileCancel'), hasLength(1));
      await tester.pumpWidget(const SizedBox());
    },
  );

  testWidgets(
    'desktop owner creates a separate share mutation with explicit grants',
    (tester) async {
      final backend = ShareBackend();
      await tester.pumpWidget(
        MaterialApp(
          theme: wispTheme(),
          home: Scaffold(
            body: SingleChildScrollView(
              child: ServiceSettings(
                backend: backend,
                servicesOnly: true,
                onChanged: () {},
              ),
            ),
          ),
        ),
      );
      await tester.pumpAndSettle();
      await tester.tap(find.byKey(const ValueKey('share-service')));
      await tester.pumpAndSettle();
      await tester.tap(find.widgetWithText(Tab, 'Share files'));
      await tester.pumpAndSettle();
      await tester.enterText(
        find.byKey(const ValueKey('share-name')),
        'checkpoints',
      );
      backend.selectedFolder = '/training/checkpoints';
      await tester.tap(find.text('Choose folder'));
      await tester.pumpAndSettle();
      await tester.ensureVisible(find.text('Training host'));
      await tester.tap(find.text('Training host'));
      await tester.ensureVisible(find.text('Save'));
      await tester.tap(find.text('Save'));
      await tester.pumpAndSettle();
      final operation = backend.calls
          .where((c) => c.$1 == 'ownerRequest')
          .single
          .$2['payload'];
      expect(operation, {
        'op': 'mutate',
        'operation': {
          'op': 'file_share',
          'id': 'checkpoints',
          'root': '/training/checkpoints',
          'allow': [host],
          'expected_revision': 7,
        },
      });
      await tester.pumpWidget(const SizedBox());
    },
  );

  testWidgets(
    'restored native downloads merge once and dismissed history stays dismissed',
    (tester) async {
      final backend = ShareBackend();
      final downloads = SharedFileDownloads(backend);
      final row = {
        ...backend.status,
        'name': 'checkpoint.bin',
        'request': {
          'host': host,
          'service': 'checkpoints',
          'incarnation': incarnation,
          'path': 'checkpoint.bin',
          'local_path': 'content://folder',
          'name': 'checkpoint.bin',
          'resume': true,
        },
      };
      downloads.restore([row]);
      downloads.restore([row]);
      expect(downloads.jobs, hasLength(1));
      backend.status = {...backend.status, 'state': 'complete'};
      await downloads.poll();
      downloads.clearFinished();
      downloads.restore([row]);
      expect(downloads.jobs, isEmpty);
      downloads.dispose();
    },
  );

  testWidgets(
    'native handle loss allows resume and superseded handles never return from snapshots',
    (tester) async {
      final backend = ShareBackend();
      final downloads = SharedFileDownloads(backend);
      final row = {
        ...backend.status,
        'name': 'checkpoint.bin',
        'request': {
          'host': host,
          'service': 'checkpoints',
          'incarnation': incarnation,
          'path': 'checkpoint.bin',
          'local_path': 'content://folder',
          'name': 'checkpoint.bin',
          'resume': true,
        },
      };
      downloads.restore([row]);
      final job = downloads.jobs.single;
      backend.statusError = const FormatException(
        'Local device is temporarily unavailable',
      );
      await downloads.poll();
      expect(job.finished, isFalse);
      expect(job.failure, contains('Waiting for the local device'));
      backend.statusError = const FormatException(
        'Download handle unavailable. Select the same file and folder to resume.',
      );
      await downloads.poll();
      expect(job.finished, isTrue);
      expect(job.status['error']['code'], 'TRANSFER_UNAVAILABLE');
      backend.statusError = null;
      backend.status = {
        ...backend.status,
        'transfer': 'd' * 64,
        'state': 'complete',
      };
      await downloads.start(job);
      downloads.restore([row]);
      expect(downloads.jobs, hasLength(1));
      expect(job.transfer, 'd' * 64);
      downloads.dispose();
    },
  );

  testWidgets(
    'share browser, folder editor and downloads fit phone and desktop widths',
    (tester) async {
      addTearDown(tester.view.resetPhysicalSize);
      addTearDown(tester.view.resetDevicePixelRatio);
      tester.view.devicePixelRatio = 1;
      for (final size in [const Size(960, 900), const Size(360, 780)]) {
        tester.view.physicalSize = size;
        final backend = ShareBackend();
        final downloads = SharedFileDownloads(backend);
        final label = size.width < 500 ? 'phone' : 'desktop';
        Future<void> preview(Widget child) async {
          await tester.pumpWidget(
            RepaintBoundary(
              key: const ValueKey('share-preview'),
              child: MaterialApp(
                debugShowCheckedModeBanner: false,
                theme: wispTheme(),
                home: Scaffold(
                  body: SingleChildScrollView(
                    padding: const EdgeInsets.all(20),
                    child: child,
                  ),
                ),
              ),
            ),
          );
          await tester.pump();
        }

        await preview(
          SharedFilesBrowser(
            backend: backend,
            machine: machine,
            service: service,
            downloads: downloads,
            desktop: true,
          ),
        );
        await captureShare(tester, '$label-share-browser');
        expect(tester.takeException(), isNull);
        await preview(
          FileShareEditor(
            backend: backend,
            clients: [
              {'peer': host, 'name': 'My laptop'},
            ],
            id: 'checkpoints',
            root: '/workspace/training/checkpoints',
            allow: [host],
            onSave: (_, _, _) async {},
          ),
        );
        await captureShare(tester, '$label-share-editor');
        expect(tester.takeException(), isNull);
        downloads.restore([
          {
            ...backend.status,
            'name': 'checkpoint.safetensors',
            'request': {'host': host, 'service': 'checkpoints'},
          },
        ]);
        await preview(SharedDownloadsList(downloads: downloads));
        await captureShare(tester, '$label-share-download');
        expect(tester.takeException(), isNull);
        downloads.dispose();
        await tester.pumpWidget(const SizedBox());
      }
    },
  );

  for (final desktop in [true, false]) {
    testWidgets(
      '${desktop ? 'desktop' : 'Android'} reveals a download immediately after picking its destination in a long share',
      (tester) async {
        addTearDown(tester.view.resetPhysicalSize);
        addTearDown(tester.view.resetDevicePixelRatio);
        tester.view.devicePixelRatio = 1;
        tester.view.physicalSize = desktop
            ? const Size(960, 900)
            : const Size(360, 780);
        final backend = LongShareBackend()
          ..selectedFolder = desktop ? '/chosen' : 'content://downloads/tree'
          ..status = {
            'transfer': 'c' * 64,
            'state': 'running',
            'phase': 'preparing',
            'verified_bytes': 0,
            'size_bytes': 0,
            'files_complete': 0,
            'files_total': 0,
          };
        final downloads = SharedFileDownloads(backend);
        addTearDown(downloads.dispose);
        final label = desktop ? 'desktop' : 'phone';
        await tester.pumpWidget(
          RepaintBoundary(
            key: const ValueKey('share-preview'),
            child: MaterialApp(
              debugShowCheckedModeBanner: false,
              theme: wispTheme(),
              home: Scaffold(
                body: Builder(
                  builder: (context) => TextButton(
                    onPressed: () => showWispSheet<void>(
                      context,
                      title: 'checkpoints on Training host',
                      child: SharedFilesBrowser(
                        backend: backend,
                        machine: machine,
                        service: service,
                        downloads: downloads,
                        desktop: desktop,
                      ),
                    ),
                    child: const Text('Browse share'),
                  ),
                ),
              ),
            ),
          ),
        );
        await tester.tap(find.text('Browse share'));
        await tester.pumpAndSettle();
        final download = find.byTooltip(
          'Download folder ${backend.lastFolder}',
        );
        // Simulate selecting a folder far below the initial viewport. After
        // the picker returns, the user must not need to find the progress card.
        await tester.ensureVisible(download);
        await tester.pump();
        await tester.tap(download);
        await tester.pump();
        await tester.pump(const Duration(milliseconds: 400));
        expect(backend.startReply.isCompleted, isFalse);
        expect(downloads.jobs, hasLength(1));
        expect(
          find.textContaining('Starting download').hitTestable(),
          findsOneWidget,
        );
        expect(
          find.byType(LinearProgressIndicator).hitTestable(),
          findsOneWidget,
        );
        expect(
          backend.calls.where((call) => call.$1 == 'shareGet'),
          hasLength(1),
        );
        await captureShare(tester, '$label-download-starting');

        backend.startReply.complete(Map.of(backend.status));
        await tester.pump();
        await tester.pump(const Duration(milliseconds: 400));
        expect(
          find.textContaining('Preparing folder').hitTestable(),
          findsOneWidget,
        );
        expect(find.textContaining('0 B / 0 B'), findsNothing);
        expect(find.text('0 / 0 files'), findsNothing);
        expect(find.text('Cancel download').hitTestable(), findsOneWidget);
        await captureShare(tester, '$label-download-preparing');

        backend.status = {
          ...backend.status,
          'phase': 'transferring',
          'verified_bytes': 512,
          'size_bytes': 1024,
          'files_complete': 1,
          'files_total': 2,
        };
        await downloads.poll();
        await tester.pump();
        expect(find.text('1 / 2 files').hitTestable(), findsOneWidget);
        final indicator = tester.widget<LinearProgressIndicator>(
          find.byType(LinearProgressIndicator),
        );
        expect(indicator.value, .5);
        await tester.tap(find.byTooltip('Close'));
        await tester.pumpAndSettle();
        expect(downloads.jobs.single.finished, isFalse);
        expect(backend.calls.where((call) => call.$1 == 'fileCancel'), isEmpty);

        // Reopening the share exposes both the ongoing task and a subsequent
        // failure without scrolling to the end of its file listing.
        await tester.tap(find.text('Browse share'));
        await tester.pump();
        await tester.pump(const Duration(milliseconds: 400));
        backend.status = {
          ...backend.status,
          'state': 'failed',
          'error': {
            'code': 'PERMISSION_DENIED',
            'message': 'Reader permission was revoked.',
          },
        };
        await downloads.poll();
        await tester.pump();
        expect(
          find.text('Reader permission was revoked.').hitTestable(),
          findsOneWidget,
        );
        expect(find.text('Resume download').hitTestable(), findsOneWidget);
        expect(tester.takeException(), isNull);
        await tester.pumpWidget(const SizedBox());
      },
    );
  }

  testWidgets(
    'a short window reveals a header-started download below the initial viewport',
    (tester) async {
      tester.view.devicePixelRatio = 1;
      tester.view.physicalSize = const Size(360, 440);
      addTearDown(tester.view.resetDevicePixelRatio);
      addTearDown(tester.view.resetPhysicalSize);
      final backend = LongShareBackend();
      final downloads = SharedFileDownloads(backend);
      addTearDown(downloads.dispose);
      await tester.pumpWidget(
        MaterialApp(
          theme: wispTheme(),
          home: Scaffold(
            body: Builder(
              builder: (context) => TextButton(
                onPressed: () => showWispSheet<void>(
                  context,
                  title: 'checkpoints on Training host',
                  child: SharedFilesBrowser(
                    backend: backend,
                    machine: machine,
                    service: service,
                    downloads: downloads,
                    desktop: true,
                  ),
                ),
                child: const Text('Browse share'),
              ),
            ),
          ),
        ),
      );
      await tester.tap(find.text('Browse share'));
      await tester.pumpAndSettle();
      final scroll = tester.state<ScrollableState>(find.byType(Scrollable));
      expect(scroll.position.pixels, 0);
      // No manual scrolling: a newly inserted card below the toolbar must be
      // revealed even though the user was already at the top of this share.
      await tester.tap(find.byTooltip('Download this folder'));
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 400));
      expect(backend.startReply.isCompleted, isFalse);
      expect(downloads.jobs.single.request['path'], '');
      expect(
        find.textContaining('Starting download').hitTestable(),
        findsOneWidget,
      );
      expect(
        find.byType(LinearProgressIndicator).hitTestable(),
        findsOneWidget,
      );
      backend.startReply.complete({...backend.status, 'state': 'complete'});
      await tester.pump();
      expect(tester.takeException(), isNull);
      await tester.pumpWidget(const SizedBox());
    },
  );

  testWidgets(
    'a failed start stays visible after revealing the pending download',
    (tester) async {
      tester.view.devicePixelRatio = 1;
      tester.view.physicalSize = const Size(360, 780);
      addTearDown(tester.view.resetDevicePixelRatio);
      addTearDown(tester.view.resetPhysicalSize);
      final backend = LongShareBackend();
      final downloads = SharedFileDownloads(backend);
      addTearDown(downloads.dispose);
      await browser(tester, backend, downloads);
      final download = find.byTooltip('Download folder ${backend.lastFolder}');
      await tester.ensureVisible(download);
      await tester.pump();
      await tester.tap(download);
      await tester.pump();
      await tester.pump(const Duration(milliseconds: 400));
      expect(
        find.textContaining('Starting download').hitTestable(),
        findsOneWidget,
      );
      backend.startReply.completeError(
        const FormatException('UNAVAILABLE: The local device stopped.'),
      );
      await tester.pump();
      expect(
        find.textContaining('The local device stopped.').hitTestable(),
        findsOneWidget,
      );
      expect(find.text('Resume download').hitTestable(), findsOneWidget);
      expect(downloads.jobs.single.status['state'], 'failed');
      expect(
        backend.calls.where((call) => call.$1 == 'shareGet'),
        hasLength(1),
      );
      await tester.pumpWidget(const SizedBox());
    },
  );

  testWidgets(
    'desktop download selects its local destination and retains job progress',
    (tester) async {
      final backend = ShareBackend();
      final downloads = SharedFileDownloads(backend);
      addTearDown(downloads.dispose);
      await browser(tester, backend, downloads);
      await tester.tap(find.byTooltip('Download checkpoint.bin'));
      await tester.pump();
      final request = backend.calls.where((c) => c.$1 == 'shareGet').single.$2;
      expect(request, {
        'host': host,
        'service': 'checkpoints',
        'incarnation': incarnation,
        'path': 'checkpoint.bin',
        'local_path': '/chosen/checkpoint.bin',
        'name': 'checkpoint.bin',
        'resume': true,
      });
      expect(find.text('Cancel download'), findsOneWidget);
      await tester.pumpWidget(const SizedBox());
      backend.status = {
        ...backend.status,
        'state': 'complete',
        'verified_bytes': 1024,
      };
      await downloads.poll();
      await browser(tester, backend, downloads);
      expect(find.text('Downloaded · 1.0 KB'), findsOneWidget);
      expect(backend.calls.any((c) => c.$1 == 'fileSend'), isFalse);
    },
  );

  testWidgets(
    'Android passes the destination capability and filename separately',
    (tester) async {
      final backend = ShareBackend()
        ..selectedFolder = 'content://downloads/opaque-folder';
      final downloads = SharedFileDownloads(backend);
      addTearDown(downloads.dispose);
      await browser(tester, backend, downloads, desktop: false);
      await tester.tap(find.byTooltip('Download checkpoint.bin'));
      await tester.pump();
      expect(
        backend.calls.where((c) => c.$1 == 'pickDownloadFolder'),
        hasLength(1),
      );
      final request = backend.calls.where((c) => c.$1 == 'shareGet').single.$2;
      expect(request['local_path'], 'content://downloads/opaque-folder');
      expect(request['name'], 'checkpoint.bin');
      backend.status = {...backend.status, 'state': 'complete'};
      await downloads.poll();
    },
  );

  for (final desktop in [true, false]) {
    testWidgets(
      '${desktop ? 'desktop' : 'Android'} downloads a folder with one destination choice and aggregate progress',
      (tester) async {
        final backend = ShareBackend()
          ..selectedFolder = desktop ? '/chosen' : 'content://downloads/tree'
          ..status = {
            'transfer': 'c' * 64,
            'state': 'running',
            'phase': 'transferring',
            'recursive': true,
            'verified_bytes': 1024,
            'size_bytes': 2048,
            'files_complete': 1,
            'files_total': 2,
          };
        final downloads = SharedFileDownloads(backend);
        addTearDown(downloads.dispose);
        await browser(tester, backend, downloads, desktop: desktop);
        await tester.tap(find.byTooltip('Download folder run-1'));
        await tester.pump();
        final request = backend.calls
            .where((c) => c.$1 == 'shareGet')
            .single
            .$2;
        expect(request['path'], 'run-1');
        expect(request['recursive'], isTrue);
        expect(
          request['local_path'],
          desktop ? '/chosen/run-1' : 'content://downloads/tree',
        );
        expect(request['name'], 'run-1');
        expect(find.text('1 / 2 files'), findsOneWidget);
        expect(find.text('Cancel download'), findsOneWidget);
        expect(
          backend.calls.where((c) => c.$1.startsWith('pick')),
          hasLength(1),
        );

        // The folder batch remains native when the browser closes, and resume
        // reuses the entire request instead of asking for each child's path.
        await tester.pumpWidget(const SizedBox());
        backend.status = {...backend.status, 'state': 'cancelled'};
        await downloads.poll();
        await browser(tester, backend, downloads, desktop: desktop);
        await tester.ensureVisible(find.text('Resume download'));
        await tester.tap(find.text('Resume download'));
        await tester.pump();
        final requests = backend.calls
            .where((c) => c.$1 == 'shareGet')
            .toList();
        expect(requests, hasLength(2));
        expect(requests.last.$2, requests.first.$2);
        expect(
          backend.calls.where((c) => c.$1.startsWith('pick')),
          hasLength(1),
        );
        backend.status = {...backend.status, 'state': 'complete'};
        await downloads.poll();
      },
    );
  }

  testWidgets('download this folder uses the current path or the share root', (
    tester,
  ) async {
    final backend = ShareBackend();
    final downloads = SharedFileDownloads(backend);
    addTearDown(downloads.dispose);
    await browser(tester, backend, downloads);
    await tester.tap(find.byTooltip('Download this folder'));
    await tester.pump();
    var request = backend.calls.where((c) => c.$1 == 'shareGet').last.$2;
    expect(request['path'], '');
    expect(request['local_path'], '/chosen/checkpoints');
    expect(request['name'], 'checkpoints');
    expect(request['recursive'], isTrue);
    backend.status = {...backend.status, 'state': 'complete'};
    await downloads.poll();
    await tester.tap(find.text('run-1').first);
    await tester.pump();
    await tester.tap(find.byTooltip('Download this folder'));
    await tester.pump();
    request = backend.calls.where((c) => c.$1 == 'shareGet').last.$2;
    expect(request['path'], 'run-1');
    expect(request['local_path'], '/chosen/run-1');
    expect(request['recursive'], isTrue);
    await downloads.poll();
  });

  testWidgets('cancelling a destination picker never starts a download', (
    tester,
  ) async {
    final backend = ShareBackend()..selectedFolder = null;
    final downloads = SharedFileDownloads(backend);
    addTearDown(downloads.dispose);
    await browser(tester, backend, downloads);
    await tester.tap(find.byTooltip('Download checkpoint.bin'));
    await tester.pump();
    expect(backend.calls.where((c) => c.$1 == 'shareGet'), isEmpty);
    expect(downloads.jobs, isEmpty);
  });

  testWidgets('cancel and resume reuse the chosen file and share', (
    tester,
  ) async {
    final backend = ShareBackend();
    final downloads = SharedFileDownloads(backend);
    addTearDown(downloads.dispose);
    await browser(tester, backend, downloads);
    await tester.tap(find.byTooltip('Download checkpoint.bin'));
    await tester.pump();
    await tester.ensureVisible(find.text('Cancel download'));
    await tester.tap(find.text('Cancel download'));
    await tester.pump();
    expect(backend.calls.last.$1, 'fileCancel');
    backend.status = {...backend.status, 'state': 'running'};
    await tester.ensureVisible(find.text('Resume download'));
    await tester.tap(find.text('Resume download'));
    await tester.pump();
    final requests = backend.calls.where((c) => c.$1 == 'shareGet').toList();
    expect(requests, hasLength(2));
    expect(requests.first.$2, requests.last.$2);
    expect(backend.calls.where((c) => c.$1 == 'pickFolder'), hasLength(1));
    backend.status = {...backend.status, 'state': 'complete'};
    await downloads.poll();
  });

  testWidgets(
    'a changed checkpoint can explicitly restart without reusing old bytes',
    (tester) async {
      final backend = ShareBackend();
      final downloads = SharedFileDownloads(backend);
      addTearDown(downloads.dispose);
      await browser(tester, backend, downloads);
      await tester.tap(find.byTooltip('Download checkpoint.bin'));
      await tester.pump();
      backend.status = {
        ...backend.status,
        'state': 'failed',
        'error': {'code': 'STALE_VERSION', 'message': 'Shared file changed'},
      };
      await downloads.poll();
      await tester.pump();
      await tester.ensureVisible(find.text('Restart download'));
      backend.status = {...backend.status, 'state': 'complete', 'error': null};
      await tester.tap(find.text('Restart download'));
      await tester.pump();
      expect(
        backend.calls.where((c) => c.$1 == 'shareGet').last.$2['resume'],
        isFalse,
      );
      await downloads.poll();
    },
  );

  testWidgets('permission denial is visible and does not offer inbox access', (
    tester,
  ) async {
    final backend = ShareBackend()..deny = true;
    final downloads = SharedFileDownloads(backend);
    addTearDown(downloads.dispose);
    await browser(tester, backend, downloads);
    expect(find.textContaining('PERMISSION_DENIED'), findsOneWidget);
    expect(find.byTooltip('Download checkpoint.bin'), findsNothing);
    expect(backend.calls, hasLength(1));
  });

  testWidgets('share editor saves only the devices selected for this share', (
    tester,
  ) async {
    final backend = ShareBackend();
    final first = 'd' * 64;
    final second = 'e' * 64;
    Map<String, dynamic>? saved;
    await tester.pumpWidget(
      MaterialApp(
        theme: wispTheme(),
        home: Scaffold(
          body: SingleChildScrollView(
            child: FileShareEditor(
              backend: backend,
              clients: [
                {'peer': first, 'name': 'Laptop'},
                {'peer': second, 'name': 'Other device'},
              ],
              id: 'checkpoints',
              root: '/training/checkpoints',
              allow: [first],
              onSave: (id, root, allow) async {
                saved = {'id': id, 'root': root, 'allow': allow};
              },
            ),
          ),
        ),
      ),
    );
    await tester.tap(find.text('Laptop'));
    await tester.tap(find.text('Other device'));
    await tester.ensureVisible(find.text('Save'));
    await tester.tap(find.text('Save'));
    await tester.pump();
    expect(saved, {
      'id': 'checkpoints',
      'root': '/training/checkpoints',
      'allow': [second],
    });
    expect(backend.calls, isEmpty);
  });
}
