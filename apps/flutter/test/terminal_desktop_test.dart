import 'dart:convert';

import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:wispnet/src/backend.dart';
import 'package:wispnet/src/terminal_sheet.dart';
import 'package:wispnet/ui.dart';

class _Backend implements WispnetBackend {
  final input = <String>[];

  @override
  Future<Map<String, dynamic>> invoke(
    String method, [
    Map<String, dynamic> arguments = const {},
  ]) async {
    if (method == 'shellInput') {
      input.add(utf8.decode(base64Decode(arguments['data_b64'] as String)));
    }
    return method == 'shellOpen' ? {'session': 'test'} : {};
  }
}

void main() {
  testWidgets('desktop hides touch keys and copies mouse-selected output', (
    tester,
  ) async {
    final backend = _Backend();
    final manager = DesktopShellManager(backend);
    final workspace = manager.openHost(
      host: 'host',
      hostName: 'Computer',
      service: 'shell',
      incarnation: 'test',
    );
    String? clipboard;
    tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
      SystemChannels.platform,
      (call) async {
        if (call.method == 'Clipboard.setData') {
          clipboard = (call.arguments as Map)['text'] as String;
        }
        return null;
      },
    );
    addTearDown(() {
      manager.closeHost('host');
      tester.binding.defaultBinaryMessenger.setMockMethodCallHandler(
        SystemChannels.platform,
        null,
      );
    });
    await tester.pumpWidget(
      MaterialApp(
        theme: wispTheme(),
        home: Scaffold(
          body: DesktopTerminalSheet(
            manager: manager,
            workspace: workspace,
            onOpenExternal: (_) async {},
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();
    expect(find.text('Ctrl-C'), findsNothing);
    expect(find.text('Esc'), findsNothing);
    final pane = workspace.activePane;
    pane.buffer.writeString('hello world\r\nsecond line');
    pane.notifyListeners();
    await tester.pump();
    final origin = tester.getTopLeft(
      find.byKey(const ValueKey('desktop-terminal-selection')),
    );
    final mouse = await tester.startGesture(
      origin + const Offset(0, 8),
      kind: PointerDeviceKind.mouse,
    );
    await mouse.moveTo(origin + const Offset(5 * 8.4, 8));
    await mouse.up();
    await tester.pump();
    await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
    await tester.sendKeyDownEvent(LogicalKeyboardKey.shiftLeft);
    await tester.sendKeyEvent(LogicalKeyboardKey.keyC);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.shiftLeft);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
    await tester.pump();
    expect(clipboard, 'hello');
    expect(backend.input, isEmpty);
    // Select backwards across two rows, then copy in line input mode.
    final reverse = await tester.startGesture(
      origin + const Offset(6 * 8.4, 25),
      kind: PointerDeviceKind.mouse,
    );
    await reverse.moveTo(origin + const Offset(6 * 8.4, 8));
    await reverse.up();
    manager.inputMode = DesktopInputMode.line;
    await tester.pump();
    await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
    await tester.sendKeyDownEvent(LogicalKeyboardKey.shiftLeft);
    await tester.sendKeyEvent(LogicalKeyboardKey.keyC);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.shiftLeft);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
    await tester.pump();
    expect(clipboard, 'world\nsecond');
    manager.inputMode = DesktopInputMode.sync;
    await tester.pump();
    await tester.tap(find.byKey(const ValueKey('desktop-terminal-selection')));
    await tester.pump();
    await tester.sendKeyDownEvent(LogicalKeyboardKey.controlLeft);
    await tester.sendKeyEvent(LogicalKeyboardKey.keyC);
    await tester.sendKeyUpEvent(LogicalKeyboardKey.controlLeft);
    await tester.pump();
    expect(backend.input, ['\x03']);
    await tester.pumpWidget(const SizedBox.shrink());
    manager.closeHost('host');
    await tester.pump();
  });
}
