import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:wispnet/ui.dart';

Future<void> mountItem(
  WidgetTester tester,
  Widget child, {
  double width = 390,
  double scale = 1,
}) async {
  tester.view.devicePixelRatio = 1;
  tester.view.physicalSize = Size(width, 800);
  addTearDown(() {
    tester.view.resetPhysicalSize();
    tester.view.resetDevicePixelRatio();
  });
  await tester.pumpWidget(
    MaterialApp(
      theme: wispTheme(),
      home: Scaffold(
        body: MediaQuery(
          data: MediaQueryData(
            size: Size(width, 800),
            textScaler: TextScaler.linear(scale),
          ),
          child: SingleChildScrollView(child: child),
        ),
      ),
    ),
  );
  await tester.pumpAndSettle();
}

void main() {
  testWidgets('service toggle keeps its status color while busy', (
    tester,
  ) async {
    var busy = false;
    var enabled = true;
    late StateSetter update;
    await mountItem(
      tester,
      StatefulBuilder(
        builder: (context, setState) {
          update = setState;
          return WispServiceItem(
            title: 'Web',
            kind: 'http',
            enabled: enabled,
            busy: busy,
            onOpen: () => setState(() => busy = true),
            onToggle: (_) {},
          );
        },
      ),
    );
    PaintPattern thumbPaint(Color color) => paints
      ..something(
        (method, arguments) =>
            method == #drawRRect &&
            (arguments[1] as Paint).color.toARGB32() == color.toARGB32(),
      );
    final paint = find.descendant(
      of: find.byType(Switch),
      matching: find.byType(CustomPaint),
    );
    expect(paint, thumbPaint(const Color(0xff4596ff)));
    await tester.tap(find.text('Web'));
    await tester.pumpAndSettle();
    expect(tester.widget<Switch>(find.byType(Switch)).onChanged, isNull);
    expect(paint, thumbPaint(const Color(0xff4596ff)));
    update(() => busy = false);
    await tester.pumpAndSettle();
    expect(paint, thumbPaint(const Color(0xff4596ff)));
    update(() => enabled = false);
    await tester.pumpAndSettle();
    expect(paint, thumbPaint(const Color(0xffff5252)));
    update(() => busy = true);
    await tester.pumpAndSettle();
    expect(paint, thumbPaint(const Color(0xffff5252)));
  });

  testWidgets('service identity stays left and settings follow the toggle', (
    tester,
  ) async {
    var opened = 0, toggled = 0, settings = 0;
    await mountItem(
      tester,
      WispServiceItem(
        title: 'Receive files',
        kind: 'files',
        card: true,
        settingsTooltip: 'File inbox',
        onOpen: () => opened++,
        onToggle: (_) => toggled++,
        onSettings: () => settings++,
      ),
    );
    final icon = find.byIcon(Icons.folder_outlined);
    final title = find.text('Receive files');
    final toggle = find.byType(WispToggle);
    final gear = find.byTooltip('File inbox');
    expect(tester.getRect(icon).right, lessThan(tester.getRect(title).left));
    expect(
      tester.getRect(toggle).right,
      lessThanOrEqualTo(tester.getRect(gear).left),
    );
    await tester.tap(gear);
    await tester.pumpAndSettle();
    expect((opened, toggled, settings), (0, 0, 1));
    await tester.tap(toggle);
    await tester.pumpAndSettle();
    expect((opened, toggled, settings), (0, 1, 1));
    await tester.tap(title);
    await tester.pumpAndSettle();
    expect((opened, toggled, settings), (1, 1, 1));
    expect(tester.takeException(), isNull);
  });

  testWidgets('disabled service can still be configured and turned back on', (
    tester,
  ) async {
    var opened = 0, settings = 0;
    bool? toggled;
    await mountItem(
      tester,
      WispServiceItem(
        title: 'Receive files',
        kind: 'files',
        enabled: false,
        available: false,
        settingsTooltip: 'File inbox',
        onOpen: () => opened++,
        onToggle: (value) => toggled = value,
        onSettings: () => settings++,
      ),
    );
    await tester.tap(find.text('Receive files'));
    await tester.pumpAndSettle();
    expect(opened, 0);
    await tester.tap(find.byTooltip('File inbox'));
    await tester.pumpAndSettle();
    expect(settings, 1);
    await tester.tap(find.byType(WispToggle));
    await tester.pumpAndSettle();
    expect(toggled, isTrue);
    expect(tester.takeException(), isNull);
  });

  testWidgets('busy service prevents every action', (tester) async {
    var actions = 0;
    await mountItem(
      tester,
      WispServiceItem(
        title: 'Receive files',
        kind: 'files',
        busy: true,
        settingsTooltip: 'File inbox',
        onOpen: () => actions++,
        onToggle: (_) => actions++,
        onSettings: () => actions++,
      ),
    );
    await tester.tap(find.text('Receive files'));
    await tester.tap(find.byTooltip('File inbox'));
    await tester.tap(find.byType(WispToggle));
    await tester.pumpAndSettle();
    expect(actions, 0);
    expect(tester.takeException(), isNull);
  });

  testWidgets('service actions fit narrow screens with larger text', (
    tester,
  ) async {
    for (final width in [240.0, 320.0]) {
      await mountItem(
        tester,
        WispServiceItem(
          title: 'A long service name that should remain readable',
          kind: 'http',
          subtitle: const Text('Public access enabled'),
          card: true,
          settingsTooltip: 'Configure service',
          onOpen: () {},
          onToggle: (_) {},
          actions: [
            IconButton(
              tooltip: 'Show links',
              onPressed: () {},
              icon: const Icon(Icons.link),
            ),
          ],
          onSettings: () {},
        ),
        width: width,
        scale: 2,
      );
      final toggle = find.byType(WispToggle);
      final link = find.byTooltip('Show links');
      final gear = find.byTooltip('Configure service');
      for (final control in [toggle, link, gear]) {
        final rect = tester.getRect(control);
        expect(rect.left, greaterThanOrEqualTo(0));
        expect(rect.right, lessThanOrEqualTo(width));
      }
      expect(
        tester.getRect(link).right,
        lessThanOrEqualTo(tester.getRect(toggle).left),
      );
      expect(
        tester.getRect(toggle).right,
        lessThanOrEqualTo(tester.getRect(gear).left),
      );
      expect(tester.takeException(), isNull);
    }
  });
}
