import 'dart:io';

import 'package:integration_test/integration_test_driver_extended.dart';

Future<void> main() => integrationDriver(
  onScreenshot: (name, bytes, [arguments]) async {
    final directory = Directory(
      Platform.environment['WISPNET_IOS_SCREENSHOTS'] ?? 'build/ios-review',
    );
    await directory.create(recursive: true);
    final filename = name.replaceAll(RegExp(r'[^a-zA-Z0-9_-]'), '_');
    await File('${directory.path}/$filename.png').writeAsBytes(bytes);
    return true;
  },
);
