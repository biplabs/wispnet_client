import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'list_item.dart';
import 'theme.dart';

/// The identifier rules are the same for custom and folder services.
class WispServiceIdField extends StatelessWidget {
  const WispServiceIdField({
    super.key,
    required this.controller,
    this.enabled = true,
    this.fieldKey,
    this.hintText,
  });

  final TextEditingController controller;
  final bool enabled;
  final Key? fieldKey;
  final String? hintText;

  @override
  Widget build(BuildContext context) => TextField(
    key: fieldKey,
    controller: controller,
    enabled: enabled,
    decoration: InputDecoration(
      labelText: 'Service ID',
      hintText: hintText,
      helperText:
          '1–64 lowercase letters, numbers, dots, underscores or hyphens',
      helperMaxLines: 3,
    ),
  );
}

/// Shared button for adding a local service.
class WispCreateButton extends StatelessWidget {
  const WispCreateButton({
    super.key,
    required this.label,
    required this.onPressed,
    this.compact = false,
  });
  final String label;
  final VoidCallback? onPressed;
  final bool compact;

  @override
  Widget build(BuildContext context) => OutlinedButton.icon(
    onPressed: onPressed,
    style: compact
        ? OutlinedButton.styleFrom(minimumSize: const Size(0, 42))
        : null,
    icon: const Icon(Icons.add, size: 20),
    label: Text(label),
  );
}

/// Command examples remain selectable; copying can omit placeholder arguments.
class WispCommandBlock extends StatelessWidget {
  const WispCommandBlock({
    super.key,
    required this.command,
    required this.copyText,
    this.copyTooltip = 'Copy command',
    this.copyMessage = 'Command copied',
  });
  final String command, copyText, copyTooltip, copyMessage;

  @override
  Widget build(BuildContext context) => Container(
    decoration: BoxDecoration(
      color: WispColors.background,
      border: Border.all(color: WispColors.border),
      borderRadius: BorderRadius.circular(12),
    ),
    child: Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 8, 4, 0),
          child: Row(
            children: [
              const Icon(Icons.terminal, size: 18, color: WispColors.muted),
              const SizedBox(width: 8),
              const Expanded(
                child: Text(
                  'Command line',
                  style: TextStyle(color: WispColors.muted),
                ),
              ),
              WispCopyButton(
                text: copyText,
                tooltip: copyTooltip,
                message: copyMessage,
              ),
            ],
          ),
        ),
        Padding(
          padding: const EdgeInsets.fromLTRB(16, 8, 16, 16),
          child: SelectableText(
            command,
            style: const TextStyle(
              fontFamily: 'monospace',
              fontSize: 14,
              height: 1.5,
              color: WispColors.cyan,
            ),
          ),
        ),
      ],
    ),
  );
}

/// Shared status-light control for service state and binary settings.
class WispToggle extends StatelessWidget {
  const WispToggle({super.key, required this.value, required this.onChanged});
  final bool value;
  final ValueChanged<bool>? onChanged;

  @override
  Widget build(BuildContext context) => Theme(
    data: ThemeData(
      useMaterial3: false,
      colorScheme: Theme.of(context).colorScheme,
    ),
    child: Switch(
      value: value,
      // Being temporarily disabled while busy must not look like being off.
      thumbColor: WidgetStateProperty.resolveWith(
        (states) => states.contains(WidgetState.selected)
            ? const Color(0xff4596ff)
            : const Color(0xffff5252),
      ),
      activeTrackColor: const Color(0xff29364a),
      inactiveTrackColor: const Color(0xff29364a),
      onChanged: onChanged,
    ),
  );
}

/// A settings row with identity on the leading side and controls at the end.
class WispToggleListTile extends StatelessWidget {
  const WispToggleListTile({
    super.key,
    required this.value,
    required this.onChanged,
    this.title,
    this.subtitle,
    this.secondary,
    this.contentPadding,
    this.actions = const [],
  });
  final bool value;
  final ValueChanged<bool?>? onChanged;
  final Widget? title, subtitle, secondary;
  final EdgeInsetsGeometry? contentPadding;
  final List<Widget> actions;

  @override
  Widget build(BuildContext context) => WispListItem(
    title: title ?? const SizedBox.shrink(),
    subtitle: subtitle,
    leading: secondary,
    enabled: onChanged != null,
    contentOpacity: onChanged == null ? .45 : 1,
    onTap: onChanged == null ? null : () => onChanged!(!value),
    contentPadding:
        contentPadding ??
        const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
    actions: [
      WispToggle(
        value: value,
        onChanged: onChanged == null ? null : (value) => onChanged!(value),
      ),
      ...actions,
    ],
  );
}

/// Settings actions always use the same icon and color.
class WispSettingsButton extends StatelessWidget {
  const WispSettingsButton({
    super.key,
    required this.tooltip,
    required this.onPressed,
  });
  final String tooltip;
  final VoidCallback? onPressed;

  @override
  Widget build(BuildContext context) => IconButton(
    tooltip: tooltip,
    onPressed: onPressed,
    icon: const Icon(Icons.settings, size: 24, color: WispColors.muted),
  );
}

/// Copy feedback stays consistent wherever an address or pairing code appears.
class WispCopyButton extends StatelessWidget {
  const WispCopyButton({
    super.key,
    required this.text,
    required this.tooltip,
    this.message = 'Copied',
    this.onCopied,
  });
  final String text, tooltip, message;
  final VoidCallback? onCopied;

  @override
  Widget build(BuildContext context) => IconButton(
    tooltip: tooltip,
    onPressed: () async {
      await Clipboard.setData(ClipboardData(text: text));
      if (!context.mounted) return;
      ScaffoldMessenger.of(context)
          .showSnackBar(SnackBar(content: Text(message)));
      onCopied?.call();
    },
    icon: const Icon(Icons.copy, color: WispColors.cyan),
  );
}
