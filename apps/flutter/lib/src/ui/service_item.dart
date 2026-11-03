import 'package:flutter/material.dart';

import 'controls.dart';
import 'list_item.dart';
import 'surfaces.dart';
import 'theme.dart';

IconData wispServiceIcon(String kind) => switch (kind) {
  'shell' => Icons.terminal,
  'files' || 'file_inbox' => Icons.move_to_inbox_outlined,
  'file_share' => Icons.folder_outlined,
  'http' => Icons.language,
  'tcp' => Icons.lan_outlined,
  _ => Icons.dns_outlined,
};

String wispServiceTypeLabel(String kind) => switch (kind) {
  'shell' => 'Shell',
  'files' || 'file_inbox' => 'File inbox',
  'file_share' => 'Shared folder',
  'http' => 'HTTP',
  'tcp' => 'TCP',
  _ => kind.toUpperCase(),
};

/// The same service identity and control order in overview cards and settings.
class WispServiceItem extends StatelessWidget {
  const WispServiceItem({
    super.key,
    required this.title,
    required this.kind,
    this.subtitle,
    this.enabled = true,
    this.busy = false,
    this.available = true,
    this.card = false,
    this.onOpen,
    this.onToggle,
    this.onSettings,
    this.settingsTooltip,
    this.toggleKey,
    this.actions = const [],
    this.contentPadding,
  });

  final String title, kind;
  final Widget? subtitle;
  final bool enabled, busy, available, card;
  final VoidCallback? onOpen, onSettings;
  final ValueChanged<bool>? onToggle;
  final String? settingsTooltip;
  final Key? toggleKey;
  final List<Widget> actions;
  final EdgeInsetsGeometry? contentPadding;

  @override
  Widget build(BuildContext context) {
    final row = WispListItem(
      title: Text(title, maxLines: 2, overflow: TextOverflow.ellipsis),
      titleStyle: const TextStyle(fontSize: 17, fontWeight: FontWeight.bold),
      subtitle: subtitle,
      leading: Icon(wispServiceIcon(kind), size: 20, color: WispColors.cyan),
      contentPadding:
          contentPadding ??
          const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
      contentOpacity: enabled && (available || onToggle != null) ? 1 : .45,
      onTap: busy
          ? null
          : onOpen != null
          ? (enabled && available ? onOpen : null)
          : onToggle == null
          ? null
          : () => onToggle!(!enabled),
      actions: [
        ...actions,
        if (onToggle != null)
          Tooltip(
            message: '${enabled ? 'Turn off' : 'Turn on'} $title',
            child: WispToggle(
              key: toggleKey,
              value: enabled,
              onChanged: busy ? null : onToggle,
            ),
          ),
        if (onSettings != null)
          WispSettingsButton(
            tooltip: settingsTooltip ?? 'Settings for $title',
            onPressed: busy ? null : onSettings,
          ),
      ],
    );
    return card
        ? WispPanel(
            color: WispColors.tile,
            radius: 16,
            padding: EdgeInsets.zero,
            borderColor: enabled && available
                ? const Color(0xff32518a)
                : const Color(0xff1d2842),
            child: row,
          )
        : row;
  }
}
