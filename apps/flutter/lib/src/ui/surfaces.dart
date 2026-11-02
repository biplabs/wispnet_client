import 'dart:ui' as ui;

import 'package:flutter/material.dart';

import 'feedback.dart';
import 'status.dart';
import 'theme.dart';

class WispPanel extends StatelessWidget {
  const WispPanel({
    super.key,
    required this.child,
    this.padding = const EdgeInsets.all(16),
    this.color = WispColors.panel,
    this.borderColor = WispColors.border,
    this.radius = 20,
  });
  final Widget child;
  final EdgeInsets padding;
  final Color color;
  final Color borderColor;
  final double radius;
  @override
  Widget build(BuildContext context) => Container(
    padding: padding,
    decoration: BoxDecoration(
      color: color,
      borderRadius: BorderRadius.circular(radius),
      border: Border.all(color: borderColor),
    ),
    child: child,
  );
}

/// Keeps device grants visibly grouped across service configuration forms.
class WispPermissionCard extends StatelessWidget {
  const WispPermissionCard({super.key, required this.children});
  final List<Widget> children;

  @override
  Widget build(BuildContext context) => WispPanel(
    radius: 16,
    padding: const EdgeInsets.fromLTRB(16, 16, 12, 8),
    child: Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        const Text('Permission', style: TextStyle(fontWeight: FontWeight.w600)),
        const SizedBox(height: 8),
        if (children.isEmpty)
          const Padding(
            padding: EdgeInsets.only(bottom: 8),
            child: Text('Pair a device to allow access.'),
          ),
        ...children,
      ],
    ),
  );
}

class WispDockBar extends StatelessWidget {
  const WispDockBar({
    super.key,
    required this.title,
    required this.statusColor,
    required this.onExpand,
    required this.onClose,
    required this.expandLabel,
    required this.closeLabel,
    this.expandKey,
    this.closeKey,
    this.showClose = true,
  });
  final String title, expandLabel, closeLabel;
  final Color statusColor;
  final VoidCallback? onExpand, onClose;
  final Key? expandKey, closeKey;
  final bool showClose;

  @override
  Widget build(BuildContext context) => WispPanel(
    color: WispColors.sheet,
    radius: 16,
    padding: const EdgeInsets.fromLTRB(6, 4, 6, 4),
    child: Row(
      children: [
        Expanded(
          child: Semantics(
            button: true,
            label: expandLabel,
            child: WispInkWell(
              key: expandKey,
              onTap: onExpand,
              borderRadius: BorderRadius.circular(12),
              child: Container(
                constraints: const BoxConstraints(minHeight: 48),
                padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 8),
                child: Row(
                  children: [
                    WispStatusDot(color: statusColor),
                    const SizedBox(width: 10),
                    Expanded(
                      child: Text(
                        title,
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: const TextStyle(
                          fontSize: 14,
                          fontWeight: FontWeight.bold,
                        ),
                      ),
                    ),
                    const SizedBox(width: 8),
                    const Icon(
                      Icons.keyboard_arrow_up,
                      size: 20,
                      color: WispColors.cyan,
                    ),
                  ],
                ),
              ),
            ),
          ),
        ),
        if (showClose) ...[
          const SizedBox(width: 4),
          IconButton(
            key: closeKey,
            tooltip: closeLabel,
            onPressed: onClose,
            icon: const Icon(Icons.close, size: 18, color: WispColors.muted),
          ),
        ],
      ],
    ),
  );
}

class _WispBottomSheetRoute<T> extends ModalBottomSheetRoute<T> {
  _WispBottomSheetRoute({
    required super.builder,
    super.capturedThemes,
    super.barrierLabel,
    super.barrierOnTapHint,
    super.backgroundColor,
    super.constraints,
    super.modalBarrierColor,
    required super.isScrollControlled,
    super.useSafeArea,
  });

  @override
  Widget buildModalBarrier() {
    final barrier = super.buildModalBarrier();
    final routeAnimation = animation;
    if (routeAnimation == null || offstage) return barrier;
    return AnimatedBuilder(
      animation: routeAnimation,
      builder: (context, child) {
        final sigma = 8 * Curves.easeOut.transform(routeAnimation.value);
        return BackdropFilter(
          filter: ui.ImageFilter.blur(sigmaX: sigma, sigmaY: sigma),
          child: child,
        );
      },
      child: barrier,
    );
  }
}

/// Whether an async completion still belongs to the visible page.
///
/// A dismissed route can remain mounted during its exit animation. Callers must
/// check this before popping after awaiting work, so they cannot pop the page
/// underneath a sheet that the user already closed.
bool wispPageIsCurrent(BuildContext context) =>
    ModalRoute.of(context)?.isCurrent ?? true;

Future<T?> showWispSheet<T>(
  BuildContext context, {
  required String title,
  required Widget child,
  bool showHeader = true,
  bool scrollable = true,
  bool fillHeight = false,
  double maxWidth = 640,
}) {
  final navigator = Navigator.of(context);
  final localizations = MaterialLocalizations.of(context);
  final nested = ModalRoute.of(context) is _WispBottomSheetRoute;
  return navigator.push(
    _WispBottomSheetRoute<T>(
      isScrollControlled: true,
      useSafeArea: true,
      backgroundColor: Colors.transparent,
      modalBarrierColor: const Color(0x78040812),
      constraints: BoxConstraints(maxWidth: maxWidth),
      capturedThemes: InheritedTheme.capture(
        from: context,
        to: navigator.context,
      ),
      barrierLabel: localizations.scrimLabel,
      barrierOnTapHint: localizations.scrimOnTapHint(
        localizations.bottomSheetLabel,
      ),
      builder: (context) {
        final content = Column(
          mainAxisSize: fillHeight ? MainAxisSize.max : MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Center(
              child: Container(
                width: 40,
                height: 4,
                margin: EdgeInsets.only(bottom: showHeader ? 12 : 6),
                decoration: BoxDecoration(
                  color: WispColors.muted.withValues(alpha: .4),
                  borderRadius: BorderRadius.circular(2),
                ),
              ),
            ),
            if (showHeader) ...[
              Row(
                children: [
                  if (nested) ...[
                    IconButton(
                      tooltip: 'Back',
                      onPressed: () => Navigator.maybePop(context),
                      icon: const Icon(Icons.arrow_back, size: 20),
                    ),
                    const SizedBox(width: 4),
                  ],
                  Expanded(
                    child: Text(
                      title,
                      style: const TextStyle(
                        fontSize: 20,
                        fontWeight: FontWeight.bold,
                      ),
                    ),
                  ),
                  IconButton(
                    tooltip: 'Close',
                    onPressed: () => navigator.popUntil(
                      (route) => route is! _WispBottomSheetRoute,
                    ),
                    icon: const Icon(Icons.close, size: 20),
                  ),
                ],
              ),
              const SizedBox(height: 12),
            ],
            Flexible(
              fit: fillHeight ? FlexFit.tight : FlexFit.loose,
              child: scrollable ? SingleChildScrollView(child: child) : child,
            ),
          ],
        );
        return Padding(
          padding: EdgeInsets.fromLTRB(
            12,
            fillHeight ? 0 : 20,
            12,
            12 + MediaQuery.viewInsetsOf(context).bottom,
          ),
          child: ConstrainedBox(
            constraints: BoxConstraints(
              maxHeight: fillHeight
                  ? double.infinity
                  : MediaQuery.sizeOf(context).height * .86,
            ),
            child: WispPanel(
              color: WispColors.sheet,
              radius: 24,
              padding: showHeader
                  ? const EdgeInsets.fromLTRB(20, 12, 20, 24)
                  : const EdgeInsets.fromLTRB(12, 8, 12, 10),
              child: content,
            ),
          ),
        );
      },
    ),
  );
}
