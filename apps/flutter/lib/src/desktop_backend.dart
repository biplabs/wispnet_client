import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:flutter/services.dart';

import 'backend.dart';
import 'desktop_qr_camera.dart';
import 'pairing_qr.dart';

/// Private pipes to the existing owner API adapter. No HTTP management server.
class DesktopBackend implements WispnetBackend {
  DesktopBackend({this.helper}) {
    shell.setMethodCallHandler((call) async {
      if (call.method == 'exit') {
        await invoke('setAccess', {'enabled': false});
      }
    });
  }
  static const shell = MethodChannel('app.wispnet/desktop');
  final String? helper;
  Future<Process>? _starting;
  Future<void>? _startup;
  final _pending = <int, Completer<Map<String, dynamic>>>{};
  int _sequence = 0;
  final _qrCamera = DesktopQrCamera();

  Future<Process> _start() async {
    final directory = File(Platform.resolvedExecutable).parent;
    final bundled = File('${directory.path}/wispnet');
    final installed = File(
      '${Platform.environment['HOME']}/.local/libexec/wispnet-node/wispnet',
    );
    final source = File('${Directory.current.path}/../../target/debug/wispnet');
    final executable =
        helper ??
        (bundled.existsSync()
            ? bundled.path
            : source.existsSync()
            ? source.path
            : installed.path);
    final process = await Process.start(executable, ['desktop-bridge']);
    process.stdout
        .transform(utf8.decoder)
        .transform(const LineSplitter())
        .listen((line) {
          try {
            final reply = jsonDecode(line) as Map<String, dynamic>;
            final completer = _pending.remove(reply['id']);
            if (completer == null) return;
            if (reply.containsKey('error')) {
              completer.completeError(
                PlatformException(
                  code: 'desktop',
                  message: reply['error'] as String,
                ),
              );
            } else {
              completer.complete(
                (reply['result'] as Map).cast<String, dynamic>(),
              );
            }
          } catch (error) {
            _fail(error);
          }
        }, onError: _fail);
    // Drain stderr; report a bounded diagnostic if the adapter exits.
    var diagnostic = '';
    process.stderr.transform(utf8.decoder).listen((text) {
      diagnostic = (diagnostic + text);
      if (diagnostic.length > 4096) {
        diagnostic = diagnostic.substring(diagnostic.length - 4096);
      }
    });
    unawaited(
      process.exitCode.then((code) {
        _starting = null;
        _fail(
          PlatformException(
            code: 'desktop',
            message: 'Desktop adapter exited ($code). ${diagnostic.trim()}',
          ),
        );
      }),
    );
    return process;
  }

  void _fail(Object error) {
    for (final request in _pending.values) {
      request.completeError(error);
    }
    _pending.clear();
  }

  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    if (method == 'qrCapabilities') {
      if (Platform.isMacOS) {
        return Map<String, dynamic>.from(
          await shell.invokeMapMethod<String, dynamic>('qrCapabilities') ?? {},
        );
      }
      return {
        'camera': (await DesktopQrCamera.linuxDevices()).isNotEmpty,
        'import': true,
      };
    }
    if (method == 'importQr') {
      final path = await shell.invokeMethod<String>('pickQrImage');
      if (path == null) return {};
      final file = File(path);
      if (await file.length() > pairingQrImageLimit) {
        throw const FormatException('Choose a QR image smaller than 4 MB.');
      }
      final builder = BytesBuilder(copy: false);
      await for (final bytes in file.openRead(0, pairingQrImageLimit + 1)) {
        builder.add(bytes);
      }
      return {'code': await decodePairingQr(builder.takeBytes())};
    }
    if (method == 'startQrCamera') {
      if (Platform.isMacOS) {
        await shell.invokeMethod<void>('startQrCamera');
      } else {
        await _qrCamera.start();
      }
      return {};
    }
    if (method == 'qrCameraFrame') {
      final bytes = Platform.isMacOS
          ? await shell.invokeMethod<Uint8List>('qrCameraFrame')
          : await _qrCamera.frame();
      if (bytes == null) {
        throw const FormatException('No camera image received.');
      }
      return {'bytes': bytes};
    }
    if (method == 'stopQrCamera') {
      if (Platform.isMacOS) {
        await shell.invokeMethod<void>('stopQrCamera');
      } else {
        await _qrCamera.stop();
      }
      return {};
    }
    if (method == 'pickFolder') {
      final path = await shell.invokeMethod<String>('pickFolder', arguments);
      return path == null ? {} : {'path': path};
    }
    if (method == 'takeLink') {
      try {
        final link = await shell.invokeMethod<String>('takeLink');
        return link == null ? {} : {'link': link};
      } on MissingPluginException {
        return {};
      }
    }
    final process = await (_starting ??= _start());
    if (method == 'snapshot') {
      await (_startup ??= invoke('startDesktop').then((_) {}));
    }
    final id = ++_sequence;
    final result = Completer<Map<String, dynamic>>();
    _pending[id] = result;
    process.stdin.writeln(
      jsonEncode({'id': id, 'method': method, 'arguments': arguments}),
    );
    final reply = await result.future.timeout(
      const Duration(seconds: 75),
      onTimeout: () {
        _pending.remove(id);
        throw PlatformException(
          code: 'timeout',
          message: 'Operation unconfirmed. Refresh before retrying.',
        );
      },
    );
    if (method == 'snapshot') {
      final hosts = (reply['hosts'] as Map?) ?? {};
      final onlinePeers = reply['enabled'] == true
          ? hosts.entries.where((entry) {
              final machine = Machine(
                entry.key as String,
                (entry.value as Map).cast<String, dynamic>(),
              );
              return entry.key != reply['peer'] && machine.online;
            }).length
          : 0;
      try {
        await shell.invokeMethod<void>('status', {
          'online': reply['enabled'] == true,
          'text':
              '$onlinePeers peers online · ${reply['activeClients'] ?? 0} active · ${reply['pendingRequests'] ?? 0} requests · Relay: ${reply['relay'] ?? 'offline'}',
        });
      } on MissingPluginException {
        /* Unit tests have no native shell. */
      }
    }
    return reply;
  }

  Future<void> close() async {
    await _qrCamera.stop();
    final process = await _starting;
    if (process != null) {
      await process.stdin.close();
      process.kill();
    }
  }
}
