import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';

import 'backend.dart';
import '../ui.dart';
import 'file_transfer.dart';

class MachineCard extends StatelessWidget {
  const MachineCard({
    super.key,
    required this.machine,
    required this.order,
    required this.telemetry,
    required this.stateColor,
    required this.busy,
    required this.onManage,
    required this.onOpen,
    required this.onLinks,
    this.onSettings,
    this.onSSHSetup,
    this.onSSHStatus,
    this.onToggle,
    required this.onReorder,
    this.showHeader = true,
    this.onFilesDropped,
  });
  final Future<String> Function()? onSSHSetup;
  final Future<Map<String, dynamic>> Function()? onSSHStatus;
  final Machine machine;
  final bool showHeader;
  final Future<void> Function(Map<String, dynamic>, List<String>)?
  onFilesDropped;
  final Map<String, dynamic> order;
  final String telemetry;
  final Color stateColor;
  final bool busy;
  final VoidCallback onManage;
  final ValueChanged<Map<String, dynamic>> onOpen, onLinks;
  final ValueChanged<Map<String, dynamic>>? onSettings;
  final void Function(Map<String, dynamic>, bool)? onToggle;
  final void Function(
    String group,
    List<String> ids,
    String moved,
    String target,
  )
  onReorder;
  @override
  Widget build(BuildContext context) {
    final services = {for (final s in machine.services) s['id'] as String: s};
    final ids = orderedIds(
      services.keys,
      order['services/${machine.id}'] as List?,
    );
    if (machine.data['local'] == true &&
        services['shell']?['kind'] == 'shell') {
      ids.remove('shell');
      ids.insert(0, 'shell');
    }
    final tel = telemetry;
    final badge = machine.online
        ? ['Online', if (tel.isNotEmpty) tel].join(' · ')
        : machine.state == 'approval_pending'
        ? 'Awaiting approval'
        : machine.state == 'host_denied'
        ? 'Access denied'
        : 'Offline';
    final content = Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        if (showHeader)
          Padding(
            padding: const EdgeInsets.symmetric(vertical: 6),
            child: LayoutBuilder(
              builder: (context, constraints) {
                final title = WispInkWell(
                  onTap: busy ? null : onManage,
                  borderRadius: BorderRadius.circular(6),
                  child: Text(
                    '${machine.name} · ${machine.id.substring(0, 8)}',
                    overflow: TextOverflow.ellipsis,
                    style: const TextStyle(
                      fontSize: 14,
                      fontWeight: FontWeight.bold,
                      letterSpacing: 14 * .08,
                    ),
                  ),
                );
                final status = Row(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    WispStatusDot(
                      color: machine.online
                          ? WispColors.cyan
                          : machine.state == 'approval_pending'
                          ? const Color(0xffe9b65c)
                          : machine.state == 'host_denied'
                          ? WispColors.error
                          : const Color(0xff7a8da8),
                    ),
                    const SizedBox(width: 6),
                    Flexible(
                      child: Text(
                        badge,
                        style: TextStyle(
                          fontSize: 11,
                          color: machine.online
                              ? const Color(0xffedf4ff)
                              : stateColor,
                          letterSpacing: 11 * .06,
                          fontWeight: FontWeight.bold,
                        ),
                      ),
                    ),
                  ],
                );
                return constraints.maxWidth < 320
                    ? Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [title, const SizedBox(height: 6), status],
                      )
                    : Row(
                        children: [
                          Expanded(
                            child: Align(
                              alignment: Alignment.centerLeft,
                              child: title,
                            ),
                          ),
                          const SizedBox(width: 12),
                          status,
                        ],
                      );
              },
            ),
          ),
        if (services.isEmpty)
          Padding(
            padding: const EdgeInsets.only(top: 2, bottom: 4),
            child: Text(
              !showHeader
                  ? 'No local services shared.'
                  : machine.data['catalog'] == null
                  ? machine.state == 'approval_pending'
                        ? 'Awaiting approval'
                        : 'Unavailable'
                  : 'No shared services',
              style: const TextStyle(color: WispColors.muted),
            ),
          ),
        LayoutBuilder(
          builder: (context, constraints) {
            final columns = constraints.maxWidth >= 520 && ids.length > 1
                ? 2
                : 1;
            final width = (constraints.maxWidth - (columns - 1) * 10) / columns;
            return Wrap(
              spacing: 10,
              runSpacing: 10,
              children: [
                for (final id in ids)
                  SizedBox(
                    width: width,
                    child: ReorderTile(
                      enabled:
                          !(machine.data['local'] == true &&
                              services[id]!['kind'] == 'shell'),
                      group: 'services/${machine.id}',
                      id: id,
                      onDrop: (moved) =>
                          onReorder('services/${machine.id}', ids, moved, id),
                      child:
                          isInboxService(services[id]!) &&
                              onFilesDropped != null
                          ? FileDropArea(
                              enabled:
                                  !busy &&
                                  machine.online &&
                                  machine.canOpen(services[id]!),
                              onFiles: (paths) =>
                                  onFilesDropped!(services[id]!, paths),
                              child: serviceCard(
                                context,
                                machine,
                                services[id]!,
                              ),
                            )
                          : serviceCard(context, machine, services[id]!),
                    ),
                  ),
              ],
            );
          },
        ),
        if (services.isNotEmpty) const SizedBox(height: 4),
      ],
    );
    return showHeader
        ? WispPanel(
            color: const Color(0xd80b1528),
            padding: const EdgeInsets.fromLTRB(16, 6, 16, 6),
            child: content,
          )
        : content;
  }

  Widget serviceCard(
    BuildContext context,
    Machine machine,
    Map<String, dynamic> service,
  ) {
    final enabled = service['enabled'] != false;
    final allowed =
        enabled &&
        ((machine.data['local'] == true && onSettings != null) ||
            machine.canOpen(service));
    final publication = service['public_access'] as Map?;
    final publicEnabled = publication?['enabled'] == true;
    final publicCached = machine.data['local'] != true && !machine.online;
    final publicOnline = !publicCached && publication?['state'] == 'online';

    final local = machine.data['local'] == true;
    final deviceCount = (service['allow'] as List? ?? [])
        .whereType<String>()
        .where((peer) => peer != machine.id)
        .toSet()
        .length;
    final subtitle = wispServiceTypeLabel(service['kind'] as String);
    final disabledIconColor = WispColors.muted.withValues(alpha: .45);
    return WispServiceItem(
      title: service['id'] as String,
      kind: service['kind'] as String,
      enabled: enabled,
      available: allowed,
      busy: busy,
      card: true,
      onOpen: () =>
          service['kind'] == 'tcp' ? onLinks(service) : onOpen(service),
      onToggle: local && onToggle != null
          ? (value) => onToggle!(service, value)
          : null,
      toggleKey: ValueKey('service-toggle-${service['id']}'),
      onSettings: onSettings == null ? null : () => onSettings!(service),
      settingsTooltip: 'Settings for ${service['id']} on ${machine.name}',
      actions: [
        if (service['kind'] == 'shell')
          IconButton(
            tooltip: 'Show connection commands for Shell',
            color: WispColors.cyan,
            disabledColor: disabledIconColor,
            onPressed: busy
                ? null
                : () => showShellCommands(
                    context,
                    machine,
                    onSetup: onSSHSetup,
                    onStatus: onSSHStatus,
                  ),
            icon: const Icon(Icons.code, size: 20),
          ),
        if (!local && isInboxService(service))
          IconButton(
            tooltip: 'Send files to ${service['id']}',
            color: WispColors.cyan,
            disabledColor: disabledIconColor,
            onPressed: busy || !allowed ? null : () => onOpen(service),
            icon: const Icon(Icons.upload_file, size: 20),
          ),
        if (!local && service['kind'] == 'file_share')
          IconButton(
            tooltip: 'Browse files in ${service['id']}',
            color: WispColors.cyan,
            disabledColor: disabledIconColor,
            onPressed: busy || !allowed ? null : () => onOpen(service),
            icon: const Icon(Icons.folder_open_outlined, size: 20),
          ),
        if (service['kind'] == 'http' || service['kind'] == 'tcp')
          IconButton(
            tooltip: 'Show links for ${service['id']}',
            color: WispColors.cyan,
            disabledColor: disabledIconColor,
            onPressed: busy ? null : () => onLinks(service),
            icon: const Icon(Icons.link, size: 20),
          ),
      ],
      subtitle: Wrap(
        spacing: 8,
        runSpacing: 4,
        crossAxisAlignment: WrapCrossAlignment.center,
        children: [
          WispBadge(label: subtitle, color: WispColors.cyan),
          if (!enabled) const Text('Off', style: TextStyle(fontSize: 11)),
          if (publicEnabled)
            Tooltip(
              message: publicCached
                  ? 'Last known public link; refresh this device to verify its status'
                  : publicOnline
                  ? 'Public access enabled and online'
                  : 'Public access enabled but offline',
              child: WispBadge(
                label: local
                    ? 'Public'
                    : publicCached
                    ? 'Public · Cached'
                    : publicOnline
                    ? 'Public · Online'
                    : 'Public · Offline',
                color: publicCached
                    ? WispColors.muted
                    : publicOnline
                    ? WispColors.green
                    : WispColors.error,
              ),
            ),
          if (local && !publicEnabled)
            Text(
              '$deviceCount ${deviceCount == 1 ? 'device' : 'devices'}',
              style: const TextStyle(fontSize: 11),
            ),
        ],
      ),
    );
  }
}

Future<void> showShellCommands(
  BuildContext context,
  Machine machine, {
  Future<String> Function()? onSetup,
  Future<Map<String, dynamic>> Function()? onStatus,
}) {
  final statusFuture = onStatus?.call();
  var added = false;
  final host = machine.id;
  final native = 'wispnet shell $host';
  final name = machine.name;
  var ssh = 'ssh shell-$name.localhost';
  var installing = false;
  String? message;
  return showWispSheet<void>(
    context,
    title: 'Connect to Shell on ${machine.name}',
    child: StatefulBuilder(
      builder: (context, update) => FutureBuilder<Map<String, dynamic>>(
        future: statusFuture,
        builder: (context, status) => Column(
          mainAxisSize: MainAxisSize.min,
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            const Text('Run on a paired computer with Shell access.'),
            const SizedBox(height: 16),
            const Text('Wispnet'),
            WispCommandBlock(command: native, copyText: native),
            const SizedBox(height: 16),
            const Text('SSH'),
            WispCommandBlock(
              command: status.data?['command'] as String? ?? ssh,
              copyText: status.data?['command'] as String? ?? ssh,
            ),
            const SizedBox(height: 16),
            if (onStatus != null && status.hasError)
              const Text(
                'Could not check SSH alias status. Reopen this popup to retry.',
              ),
            if (onStatus != null &&
                status.hasData &&
                status.data?['installed'] != true)
              const Text(
                'Update Wispnet on this computer to enable SSH connections.',
              ),
            if (onSetup != null &&
                !added &&
                status.data?['alias_configured'] != true) ...[
              const SizedBox(height: 8),
              FilledButton.icon(
                icon: const Icon(Icons.add_link),
                label: Text(installing ? 'Adding…' : 'Add SSH alias'),
                onPressed:
                    (installing ||
                        added ||
                        status.data?['alias_configured'] == true ||
                        (onStatus != null &&
                            (!status.hasData ||
                                status.data?['installed'] != true)))
                    ? null
                    : () async {
                        update(() {
                          installing = true;
                          message = null;
                        });
                        try {
                          final command = await onSetup();
                          if (context.mounted) {
                            update(() {
                              ssh = command;
                              added = true;
                              message = null;
                            });
                          }
                        } catch (error) {
                          if (context.mounted) {
                            update(() {
                              message = 'Could not add SSH alias: $error';
                            });
                          }
                        } finally {
                          if (context.mounted) {
                            update(() {
                              installing = false;
                            });
                          }
                        }
                      },
              ),
              if (message != null) Text(message!),
              const SizedBox(height: 8),
            ],
          ],
        ),
      ),
    ),
  );
}

class ReorderTile extends StatelessWidget {
  const ReorderTile({
    super.key,
    required this.group,
    this.enabled = true,
    required this.id,
    required this.onDrop,
    required this.child,
  });
  final String group, id;
  final bool enabled;
  final ValueChanged<String> onDrop;
  final Widget child;
  @override
  Widget build(BuildContext context) => !enabled
      ? child
      : DragTarget<String>(
          onWillAcceptWithDetails: (d) =>
              d.data.startsWith('$group\n') && d.data != '$group\n$id',
          onAcceptWithDetails: (d) =>
              onDrop(d.data.substring(group.length + 1)),
          builder: (context, candidates, rejected) {
            final feedback = Material(
              color: Colors.transparent,
              child: WispPanel(child: Text(id)),
            );
            final content = AnimatedOpacity(
              duration: const Duration(milliseconds: 120),
              opacity: candidates.isEmpty ? 1 : .65,
              child: child,
            );
            final touchPlatform =
                defaultTargetPlatform == TargetPlatform.android ||
                defaultTargetPlatform == TargetPlatform.iOS;
            if (touchPlatform) {
              return LongPressDraggable<String>(
                data: '$group\n$id',
                maxSimultaneousDrags: 1,
                feedback: feedback,
                childWhenDragging: Opacity(opacity: .4, child: child),
                child: content,
              );
            }
            return Draggable<String>(
              data: '$group\n$id',
              maxSimultaneousDrags: 1,
              feedback: feedback,
              childWhenDragging: Opacity(opacity: .4, child: child),
              child: content,
            );
          },
        );
}
