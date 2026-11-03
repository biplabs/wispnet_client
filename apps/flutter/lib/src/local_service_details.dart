import 'package:flutter/material.dart';

import '../ui.dart';
import 'file_transfer.dart';

/// The same service information and actions on every hosting device.
class LocalServiceDetails extends StatelessWidget {
  const LocalServiceDetails({
    super.key,
    required this.id,
    required this.kind,
    required this.host,
    required this.onEdit,
    required this.onRemove,
    this.removeLabel = 'Remove service',
    this.busy = false,
    this.links = const [],
    this.notices = const [],
    this.extraActions = const [],
  });

  final String id, kind, host, removeLabel;
  final bool busy;
  final VoidCallback? onEdit, onRemove;
  final List<Widget> links, notices, extraActions;

  Widget action(String label, VoidCallback? callback, {bool danger = false}) =>
      Padding(
        padding: const EdgeInsets.only(bottom: 8),
        child: OutlinedButton(
          onPressed: busy ? null : callback,
          style: danger ? wispDangerButtonStyle() : null,
          child: Text(label),
        ),
      );

  @override
  Widget build(BuildContext context) => WispPanel(
    child: Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Text(
          id,
          style: const TextStyle(fontSize: 18, fontWeight: FontWeight.bold),
        ),
        const SizedBox(height: 8),
        if (kind == 'shell') ...[
          const Text(
            'Open Shell on this device from a paired device with Shell access, or run this command on a paired computer.',
          ),
          const SizedBox(height: 12),
          WispCommandBlock(
            command: 'wispnet shell $host',
            copyText: 'wispnet shell $host',
          ),
        ] else if (kind == 'files' || kind == 'file_inbox') ...[
          const Text(
            'Send files from a paired device with Files access. This device chooses where received files are saved. Replace /local/file with the file to send and filename with its name.',
          ),
          const SizedBox(height: 12),
          FileSendCommand(host: host, service: id),
        ] else if (kind == 'file_share')
          const Text(
            'Open this folder from a paired device with access to browse and download its shared files.',
          ),
        ...links,
        ...notices,
        const SizedBox(height: 10),
        action('Edit', onEdit),
        action(removeLabel, onRemove, danger: true),
        ...extraActions,
      ],
    ),
  );
}
