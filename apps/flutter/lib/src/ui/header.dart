import 'package:flutter/material.dart';

import 'feedback.dart';
import 'theme.dart';

/// Shared device header and browser-access toggle geometry.
class WispHeader extends StatelessWidget {
  const WispHeader({
    super.key,
    required this.label,
    required this.online,
    required this.onSettings,
    required this.onToggle,
  });
  final String label;
  final bool online;
  final VoidCallback? onSettings, onToggle;

  @override
  Widget build(BuildContext context) => SizedBox(
    height: 64,
    child: Row(
      children: [
        Expanded(
          child: Align(
            alignment: Alignment.centerLeft,
            child: Transform.translate(
              offset: const Offset(0, 4),
              child: Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  Flexible(
                    child: WispInkWell(
                      onTap: onSettings,
                      child: Text(
                        label,
                        key: const ValueKey('device-heading'),
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: const TextStyle(
                          fontSize: 21,
                          fontWeight: FontWeight.bold,
                        ),
                      ),
                    ),
                  ),
                  SizedBox.square(
                    dimension: 44,
                    child: IconButton(
                      tooltip: 'Settings',
                      onPressed: onSettings,
                      padding: const EdgeInsets.all(10),
                      constraints: const BoxConstraints.tightFor(
                        width: 44,
                        height: 44,
                      ),
                      icon: const Icon(
                        Icons.settings,
                        size: 24,
                        color: WispColors.muted,
                      ),
                    ),
                  ),
                ],
              ),
            ),
          ),
        ),
        if (online)
          Transform.translate(
            offset: const Offset(0, 6),
            child: OnlineButton(onPressed: onToggle),
          ),
      ],
    ),
  );
}

/// Shared online status pill. Clicking it switches access off.
class OnlineButton extends StatelessWidget {
  const OnlineButton({super.key, required this.onPressed});
  final VoidCallback? onPressed;

  static const labelBaselineOffset = 11.5 * (1900 - 500) / (2 * 2048);

  static TextPainter labelPainter() => TextPainter(
    text: const TextSpan(
      text: 'Online',
      style: TextStyle(
        fontFamily: 'Roboto',
        fontSize: 11.5,
        fontWeight: FontWeight.bold,
        letterSpacing: 11.5 * .08,
        color: Color(0xffedf4ff),
      ),
    ),
    textDirection: TextDirection.ltr,
    textScaler: TextScaler.noScaling,
  )..layout();

  @override
  Widget build(BuildContext context) {
    final density = MediaQuery.devicePixelRatioOf(context);
    final label = labelPainter();
    final width = ((label.width + 48) * density).ceil() / density;
    label.dispose();
    final height = (44 * density).round() / density;
    return Semantics(
      label: 'Online',
      hint: 'Switch offline',
      toggled: true,
      enabled: onPressed != null,
      onTap: onPressed,
      child: ExcludeSemantics(
        child: Tooltip(
          message: 'Go offline',
          child: WispInkWell(
            onTap: onPressed,
            customBorder: const StadiumBorder(),
            child: RepaintBoundary(
              key: const ValueKey('offline-control'),
              child: CustomPaint(
                size: Size(width, height),
                painter: const _OfflinePill(),
              ),
            ),
          ),
        ),
      ),
    );
  }
}

class _OfflinePill extends CustomPainter {
  const _OfflinePill();
  @override
  void paint(Canvas canvas, Size size) {
    const x = 2.0, height = 38.0;
    final y = (size.height - height) / 2, width = size.width - 4;
    final paint = Paint()..isAntiAlias = true;
    paint.color = const Color(0x334582ff);
    canvas.drawRRect(
      RRect.fromRectAndRadius(
        Rect.fromLTWH(x - 2, y - 2, width + 4, height + 4),
        const Radius.circular(21),
      ),
      paint,
    );
    paint.color = const Color(0xd812254a);
    final pill = RRect.fromRectAndRadius(
      Rect.fromLTWH(x, y, width, height),
      const Radius.circular(19),
    );
    canvas.drawRRect(pill, paint);
    paint
      ..style = PaintingStyle.stroke
      ..strokeWidth = 1.4
      ..color = const Color(0xff5b9cf7);
    canvas.drawRRect(pill, paint);
    final text = OnlineButton.labelPainter();
    final contentWidth = 8.4 + 8 + text.width;
    final bx = (size.width - contentWidth) / 2 + 4.2, by = y + height / 2;
    paint
      ..style = PaintingStyle.fill
      ..color = const Color(0x4465ceff);
    canvas.drawCircle(Offset(bx, by), 7.5, paint);
    paint.color = const Color(0xff65ceff);
    canvas.drawCircle(Offset(bx, by), 4.2, paint);
    paint.color = const Color(0xfff4fcff);
    canvas.drawCircle(Offset(bx, by), 1.8, paint);
    canvas.save();
    canvas.translate(
      bx + 12.2,
      by +
          OnlineButton.labelBaselineOffset -
          text.computeDistanceToActualBaseline(TextBaseline.alphabetic),
    );
    text.paint(canvas, Offset.zero);
    canvas.restore();
    text.dispose();
  }

  @override
  bool shouldRepaint(_OfflinePill oldDelegate) => false;
}

/// Section titles and adjacent actions share the directory's spacing and type.
class WispSectionHeading extends StatelessWidget {
  const WispSectionHeading({
    super.key,
    required this.title,
    this.actions = const [],
    this.trailing,
  });
  final String title;
  final List<Widget> actions;
  final Widget? trailing;

  @override
  Widget build(BuildContext context) {
    final heading = Row(
      children: [
        Flexible(
          child: Text(
            title,
            style: const TextStyle(
              fontSize: 16,
              color: WispColors.muted,
              letterSpacing: 1.4,
              fontWeight: FontWeight.bold,
            ),
          ),
        ),
        const SizedBox(width: 12),
        ...actions,
      ],
    );
    return trailing == null
        ? heading
        : Row(
            children: [
              Expanded(child: heading),
              trailing!,
            ],
          );
  }
}
