import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../ui.dart';
import 'backend.dart';
import 'pairing_qr.dart';

/// Shared camera preview and controls; capture belongs to the platform adapter.
class QrCameraSheet extends StatefulWidget {
  const QrCameraSheet({super.key, required this.backend});
  final WispnetBackend backend;
  @override
  State<QrCameraSheet> createState() => _QrCameraSheetState();
}

class _QrCameraSheetState extends State<QrCameraSheet>
    with WidgetsBindingObserver {
  Uint8List? preview;
  String? failure;
  bool scanning = false;
  int generation = 0;
  bool foreground = true;
  Future<void>? stopping;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    unawaited(scan());
  }

  Future<void> scan() async {
    final current = ++generation;
    setState(() {
      scanning = true;
      failure = null;
    });
    try {
      await stopping;
      if (!mounted || current != generation || !foreground) return;
      await widget.backend.invoke('startQrCamera');
      final deadline = DateTime.now().add(const Duration(seconds: 45));
      while (mounted &&
          current == generation &&
          DateTime.now().isBefore(deadline)) {
        final result = await widget.backend.invoke('qrCameraFrame');
        if (!mounted || current != generation) return;
        final bytes = result['bytes'] as Uint8List;
        setState(() => preview = bytes);
        try {
          final code = await decodePairingQr(
            bytes,
            processingTimeout: const Duration(seconds: 1),
          );
          if (mounted && current == generation && wispPageIsCurrent(context)) {
            Navigator.pop(context, code);
          }
          return;
        } on FormatException {
          // Keep looking while the user aligns a pairing QR with the camera.
        }
      }
      if (mounted && current == generation) {
        setState(
          () => failure = 'No pairing QR found. Try again or import its image.',
        );
      }
    } catch (error) {
      if (mounted && current == generation) {
        setState(
          () => failure = error is FormatException
              ? error.message
              : error is PlatformException
              ? error.message ?? 'Camera unavailable. Try again.'
              : 'Camera unavailable. Check camera access and try again.',
        );
      }
    } finally {
      if (current == generation) await stopCamera();
      if (mounted && current == generation) setState(() => scanning = false);
    }
  }

  Future<void> stopCamera() {
    final previous = stopping;
    if (previous != null) return previous;
    late final Future<void> close;
    close = stopNativeCamera().whenComplete(() {
      if (identical(stopping, close)) stopping = null;
    });
    stopping = close;
    return close;
  }

  Future<void> stopNativeCamera() async {
    try {
      await widget.backend.invoke('stopQrCamera');
    } catch (_) {
      // A disappearing native window must not turn Cancel into an async error.
    }
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    if (state == AppLifecycleState.hidden ||
        state == AppLifecycleState.paused ||
        state == AppLifecycleState.detached) {
      foreground = false;
      generation++;
      unawaited(stopCamera());
      if (mounted) {
        setState(() {
          scanning = false;
          preview = null;
          failure = 'Camera paused. Return to Wispnet and try again.';
        });
      }
    } else if (state == AppLifecycleState.resumed) {
      if (mounted) setState(() => foreground = true);
    }
  }

  @override
  void dispose() {
    generation++;
    WidgetsBinding.instance.removeObserver(this);
    unawaited(stopCamera());
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => Column(
    crossAxisAlignment: CrossAxisAlignment.stretch,
    children: [
      const Text('Point your camera at a Wispnet pairing QR.'),
      const SizedBox(height: 12),
      Center(
        child: ConstrainedBox(
          // Leave room for the sheet header and controls on short screens.
          constraints: BoxConstraints(
            maxWidth: (MediaQuery.sizeOf(context).height * .86 - 220)
                .clamp(120, 420)
                .toDouble(),
          ),
          child: AspectRatio(
            aspectRatio: 1,
            child: Container(
              clipBehavior: Clip.antiAlias,
              decoration: BoxDecoration(
                color: WispColors.background,
                borderRadius: BorderRadius.circular(16),
              ),
              foregroundDecoration: BoxDecoration(
                border: Border.all(
                  color: WispColors.muted.withValues(alpha: .5),
                ),
                borderRadius: BorderRadius.circular(16),
              ),
              child: preview == null
                  ? Center(
                      child: scanning
                          ? const CircularProgressIndicator()
                          : const Icon(
                              Icons.videocam_off_outlined,
                              color: WispColors.muted,
                            ),
                    )
                  : Image.memory(
                      preview!,
                      gaplessPlayback: true,
                      fit: BoxFit.cover,
                    ),
            ),
          ),
        ),
      ),
      if (failure != null) ...[
        const SizedBox(height: 12),
        Text(failure!, style: const TextStyle(color: WispColors.error)),
      ],
      const SizedBox(height: 12),
      if (!scanning)
        OutlinedButton(
          onPressed: foreground ? scan : null,
          child: const Text('Try again'),
        ),
      TextButton(
        onPressed: () => Navigator.pop(context),
        child: const Text('Cancel'),
      ),
    ],
  );
}
