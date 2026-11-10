import 'dart:convert';

import 'package:flutter/services.dart';

/// UI-facing operations. Platform adapters own identity, persistence and access.
abstract interface class WispnetBackend {
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]);
}

class PlatformBackend implements WispnetBackend {
  static const channel = MethodChannel('app.wispnet/ui');
  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    final reply = await channel.invokeMethod<String>(
      method,
      jsonEncode(arguments),
    );
    final result = jsonDecode(reply ?? '{}') as Map<String, dynamic>;
    if (method == 'qrCameraFrame') {
      final encoded = result.remove('image_b64');
      if (encoded is! String ||
          encoded.length > (4 * 1024 * 1024 * 4 ~/ 3) + 4) {
        throw const FormatException('Invalid camera image.');
      }
      final bytes = base64Decode(encoded);
      if (bytes.isEmpty || bytes.length > 4 * 1024 * 1024) {
        throw const FormatException('Invalid camera image.');
      }
      result['bytes'] = bytes;
    }
    return result;
  }
}

class Machine {
  Machine(this.id, this.data);
  final String id;
  final Map<String, dynamic> data;
  String get name => data['name'] as String? ?? 'device-${id.substring(0, 8)}';
  String get state => data['state'] as String? ?? 'unreachable';
  String get status => data['status'] as String? ?? 'Unavailable';
  bool get online => state == 'available';
  List<Map<String, dynamic>> get services {
    final metadata = data['public_access'] as Map?;
    final links =
        (metadata?['host'] == id && metadata?['version'] == 1
                ? (metadata?['services'] as List? ?? [])
                : <dynamic>[])
            .cast<Map<String, dynamic>>();
    return ((data['catalog'] as Map?)?['services'] as List? ?? [])
        .cast<Map<String, dynamic>>()
        .map((service) {
          final publication = links
              .where(
                (link) =>
                    link['service'] == service['id'] &&
                    link['incarnation'] == service['incarnation'],
              )
              .firstOrNull;
          return {
            ...service,
            if (publication != null && service['kind'] == 'http')
              'public_access': {...publication, 'enabled': true},
          };
        })
        .toList();
  }

  bool canOpen(Map<String, dynamic> service) =>
      service['enabled'] != false &&
      const [
        'http',
        'tcp',
        'shell',
        'files',
        'file_inbox',
        'file_share',
      ].contains(service['kind']) &&
      RegExp(r'^[0-9a-f]{64}$')
          .hasMatch(service['incarnation'] as String? ?? '') &&
      ![
        'host_denied',
        'approval_pending',
        'state_changed',
        'unverified',
      ].contains(state);
}

bool isInboxService(Map<String, dynamic> service) =>
    const ['files', 'file_inbox'].contains(service['kind']);

bool isFolderService(Map<String, dynamic> service) =>
    isInboxService(service) || service['kind'] == 'file_share';

List<String> orderedIds(Iterable<String> ids, List<dynamic>? saved) {
  final available = ids.toSet();
  return {
    ...?saved?.whereType<String>().where(available.contains),
    ...ids,
  }.toList();
}
