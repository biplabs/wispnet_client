import 'dart:async';
import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'backend.dart';
import 'space.dart';
import '../ui.dart';
import 'sheets.dart';
import 'cards.dart';
import 'service_settings.dart';
import 'terminal_sheet.dart';
import 'file_transfer.dart';
import 'file_shares.dart';

class WispnetApp extends StatelessWidget {
  const WispnetApp({super.key, required this.backend});
  final WispnetBackend backend;
  @override
  Widget build(BuildContext context) => MaterialApp(
    title: 'Wispnet',
    debugShowCheckedModeBanner: false,
    theme: wispTheme(),
    home: WispHome(backend: backend),
  );
}

class WispHome extends StatefulWidget {
  const WispHome({super.key, required this.backend});
  final WispnetBackend backend;
  @override
  State<WispHome> createState() => _WispHomeState();
}

class _WispHomeState extends State<WispHome>
    with SingleTickerProviderStateMixin, WidgetsBindingObserver {
  Map<String, dynamic> snapshot = {};
  String? initializationError;
  bool polling = false,
      pollQueued = false,
      refreshing = false,
      busy = false,
      foreground = true,
      pairingSheet = false,
      entering = false,
      sharing = false;
  int ticks = 0, launchGeneration = 0;
  String lastNetwork = '';
  Timer? timer;
  late final AnimationController warp = AnimationController(
    vsync: this,
    duration: const Duration(milliseconds: 900),
  );
  late final DesktopShellManager desktopShells =
      DesktopShellManager(widget.backend)..addListener(() {
        if (mounted) setState(() {});
      });
  bool get online => snapshot['enabled'] == true;
  late final SharedFileDownloads sharedDownloads =
      SharedFileDownloads(widget.backend)..addListener(() {
        if (mounted) setState(() {});
      });
  List<Map<String, dynamic>> get minimizedShells =>
      online ? desktopShells.minimizedSnapshots : const [];

  Map<String, dynamic> get records =>
      (snapshot['hosts'] as Map?)?.cast<String, dynamic>() ?? {};
  Map<String, dynamic> get order =>
      (snapshot['order'] as Map?)?.cast<String, dynamic>() ?? {};
  List<Machine> get machines =>
      orderedIds(records.keys, order['machines'] as List?)
          .map(
            (id) => Machine(id, (records[id] as Map).cast<String, dynamic>()),
          )
          .toList();
  WispnetBackend get backend => widget.backend;
  late final fileTransfers = DesktopFileTransfers(backend)
    ..addListener(() {
      if (mounted) setState(() {});
    });

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addObserver(this);
    unawaited(initialize());
    timer = Timer.periodic(const Duration(seconds: 2), (_) {
      if (!foreground) return;
      unawaited(poll());
      if (++ticks % 8 == 0) unawaited(refresh());
    });
  }

  Future<void> initialize() async {
    try {
      await poll(propagate: true);
      if (mounted) {
        setState(() => initializationError = null);
        unawaited(refresh());
      }
    } catch (error) {
      if (mounted) setState(() => initializationError = message(error));
    }
  }

  @override
  void didChangeAppLifecycleState(AppLifecycleState state) {
    foreground =
        state == AppLifecycleState.resumed ||
        snapshot['desktop'] == true && state == AppLifecycleState.inactive;
    if (foreground) {
      unawaited(poll());
      unawaited(refresh());
    } else {
      launchGeneration++;
    }
  }

  @override
  void dispose() {
    timer?.cancel();
    desktopShells.closeAll(terminate: false, notify: false);
    desktopShells.dispose();
    fileTransfers.dispose();
    sharedDownloads.dispose();
    WidgetsBinding.instance.removeObserver(this);
    warp.dispose();
    super.dispose();
  }

  String message(Object error) => error is PlatformException
      ? error.message ?? error.code
      : error is MissingPluginException
      ? 'This platform adapter is not available yet.'
      : error.toString();
  void notice(String text) {
    if (!mounted) return;
    ScaffoldMessenger.of(context)
      ..hideCurrentSnackBar()
      ..showSnackBar(SnackBar(content: Text(text)));
  }

  Future<void> poll({bool propagate = false}) async {
    if (polling) {
      pollQueued = true;
      return;
    }
    polling = true;
    try {
      final next = await backend.invoke('snapshot');
      if (!mounted) return;
      final network = next['network'] as String? ?? '';
      if (next['enabled'] == true &&
          network != lastNetwork &&
          network != 'Network available' &&
          network.isNotEmpty) {
        notice(network);
      }
      lastNetwork = network;
      desktopShells.android = next['desktop'] != true;
      if (desktopShells.android && next['enabled'] == true) {
        final saved = next['terminal_layout'] as Map?;
        desktopShells.restoreLayout(
          saved?.cast<String, dynamic>() ??
              {
                'workspaces': [
                  for (final shell
                      in (next['shells'] as List? ??
                              [if (next['shell'] is Map) next['shell']])
                          .whereType<Map>())
                    {
                      ...shell,
                      'tabs': [
                        {
                          'id': shell['session'] ?? 'main',
                          'panes': [shell['session'] ?? 'main'],
                        },
                      ],
                    },
                ],
              },
        );
      }
      if (next['hosts'] is Map) {
        final validHosts = (next['hosts'] as Map).keys
            .whereType<String>()
            .toSet();
        desktopShells.pruneRemovedHosts(validHosts);
      }
      setState(() {
        snapshot = next;
        if (next['desktop'] == true &&
            WidgetsBinding.instance.lifecycleState ==
                AppLifecycleState.inactive) {
          foreground = true;
        }
        initializationError = null;
      });
      fileTransfers.nativeQueue = next['desktop'] != true;
      sharedDownloads.restore(next['share_downloads'] as List? ?? []);
      if (next['desktop'] != true && next['enabled'] == true) {
        unawaited(
          fileTransfers.restore({
            for (final machine in machines) machine.id: machine,
          }),
        );
      }
      if (pairingSheet) return;
      final link = (await backend.invoke('takeLink'))['link'] as String?;
      if (link != null) {
        try {
          final uri = Uri.parse(link);
          if (uri.scheme != 'wispnet' ||
              !RegExp(r'^[0-9a-f]{64}$').hasMatch(uri.host) ||
              uri.userInfo.isNotEmpty ||
              uri.hasPort ||
              uri.hasQuery ||
              uri.hasFragment ||
              !RegExp(r'^/[a-z0-9][a-z0-9._-]{0,63}$').hasMatch(uri.path)) {
            throw const FormatException('Invalid Wispnet service link');
          }
          await backend.invoke('setAccess', {'enabled': true});
          final resolved = await backend.invoke('resolveLink', {'link': link});
          if (resolved['kind'] == 'tcp') {
            if (!mounted) return;
            await showWispSheet<void>(
              context,
              title: 'TCP connection',
              child: WispPanel(
                child: Column(
                  crossAxisAlignment: CrossAxisAlignment.stretch,
                  children: [
                    const Text(
                      'Connect an app on this device to this address while Wispnet is online.',
                    ),
                    const SizedBox(height: 12),
                    SelectableText(resolved['address'] as String),
                    WispCopyButton(
                      text: resolved['address'] as String,
                      tooltip: 'Copy TCP address',
                    ),
                  ],
                ),
              ),
            );
          } else {
            await backend.invoke('launch', {'url': resolved['url']});
          }
        } catch (error) {
          notice(message(error));
        }
      }
      if (snapshot['desktop'] != true && !sharing) {
        try {
          final share = await backend.invoke('takeShare');
          final files = (share['files'] as List? ?? const [])
              .cast<Map<String, dynamic>>();
          if (files.isNotEmpty && mounted) {
            sharing = true;
            unawaited(receiveSharedFiles(files));
            return;
          }
        } catch (error) {
          if (error is! MissingPluginException) notice(message(error));
        }
      }
      final invitation = await backend.invoke('takeInvitation');
      final invitationError = invitation['error'] as String?;
      if (invitationError != null) notice(invitationError);
      final code = invitation['code'] as String?;
      if (code != null && code.isNotEmpty && mounted && !pairingSheet) {
        unawaited(pair(code: code));
      }
    } catch (error) {
      if (propagate) rethrow;
    } finally {
      polling = false;
      if (pollQueued && mounted) {
        pollQueued = false;
        unawaited(poll());
      }
    }
  }

  Future<void> refresh() async {
    if (refreshing || snapshot.isEmpty || !mounted) return;
    setState(() => refreshing = true);
    final remotes = machines.where((m) => m.data['local'] != true).toList();
    if (remotes.isEmpty) {
      await poll();
    } else {
      await Future.wait(
        remotes.map((machine) async {
          try {
            await backend.invoke('refreshHost', {'host': machine.id});
            await poll();
          } catch (error) {
            notice(message(error));
          }
        }),
      );
    }
    if (mounted) setState(() => refreshing = false);
  }

  Future<void> toggle() async {
    if (busy) return;
    final enable = !online;
    setState(() {
      busy = true;
      entering = enable && !MediaQuery.disableAnimationsOf(context);
    });
    final jump = entering ? warp.forward(from: 0) : null;
    try {
      await backend.invoke('setAccess', {'enabled': enable});
      if (enable) {
        final deadline = DateTime.now().add(const Duration(seconds: 15));
        while (mounted) {
          await poll();
          if (online) break;
          if (DateTime.now().isAfter(deadline)) {
            await backend.invoke('setAccess', {'enabled': false});
            throw StateError('Access could not start. Try again.');
          }
          await Future<void>.delayed(const Duration(milliseconds: 200));
        }
        if (mounted && jump != null) await jump;
        unawaited(refresh());
      } else {
        desktopShells.closeAll(terminate: true);
        await Future<void>.delayed(const Duration(milliseconds: 100));
        await poll();
      }
    } catch (error) {
      notice(message(error));
    } finally {
      if (mounted) {
        if (jump != null) warp.reset();
        setState(() {
          busy = false;
          entering = false;
        });
      }
    }
  }

  Future<void> showDesktopShell({
    required String host,
    required String hostName,
    required String service,
    required String incarnation,
    String session = 'main',
  }) async {
    final workspace = desktopShells.openHost(
      host: host,
      hostName: hostName,
      service: service,
      incarnation: incarnation,
      session: session,
    );
    if (!mounted) return;
    await showWispSheet<void>(
      context,
      title: hostName,
      showHeader: false,
      scrollable: false,
      fillHeight: true,
      maxWidth: 1120,
      child: DesktopTerminalSheet(
        manager: desktopShells,
        workspace: workspace,
        onOpenExternal: (pane) async {
          try {
            await backend.invoke('openShell', {
              'host': pane.host,
              'host_name': pane.hostName,
              'service': pane.service,
              'incarnation': pane.incarnation,
              'session': pane.session,
            });
          } catch (error) {
            notice('Cannot open terminal: ${message(error)}');
          }
        },
      ),
    );
    if (workspace.tabs.isNotEmpty) {
      workspace.minimized = true;
      desktopShells.persistLayout();
    }
    if (mounted) setState(() {});
  }

  Future<void> showFiles(
    Machine machine,
    Map<String, dynamic> service, {
    List<String> paths = const [],
    List<Map<String, dynamic>> files = const [],
  }) async {
    if (machine.data['local'] != true) {
      final submitted = <DesktopFileTransfer>[];
      await showWispSheet<void>(
        context,
        title: service['id'] == 'files'
            ? 'Send files to ${machine.name}'
            : 'Send files to ${machine.name} / ${service['id']}',
        child: FileSendSheet(
          backend: backend,
          machine: machine,
          incarnation: service['incarnation'] as String,
          service: service['id'] as String,
          transfers: fileTransfers,
          initialPaths: paths,
          initialFiles: files,
          onQueued: submitted.addAll,
          desktop: snapshot['desktop'] == true,
        ),
      );
      // Native drop providers must remain alive if the user hides the sheet
      // while the daemon is still reading the dropped files.
      await Future.wait(submitted.map((job) => job.done));
      return;
    }
    await showWispSheet<void>(
      context,
      title: 'Send files to ${machine.name}',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          const Text(
            'Open this inbox from the Wispnet app on a paired device to send files, or run this command on a paired computer. Transfers resume after disconnects.',
          ),
          const SizedBox(height: 12),
          const Text(
            'Replace /local/file with the file to send and filename with its name. This device chooses where received files are saved.',
          ),
          const SizedBox(height: 16),
          const Text('Send a file'),
          const SizedBox(height: 8),
          FileSendCommand(host: machine.id, service: service['id'] as String),
        ],
      ),
    );
  }

  Future<void> open(Machine machine, Map<String, dynamic> service) async {
    if (busy || !online) return;
    if (service['kind'] == 'file_share') {
      await showWispSheet<void>(
        context,
        title: '${service['id']} on ${machine.name}',
        child: SharedFilesBrowser(
          backend: backend,
          machine: machine,
          service: service,
          downloads: sharedDownloads,
          desktop: snapshot['desktop'] == true,
        ),
      );
      return;
    }
    if (isInboxService(service)) {
      await showFiles(machine, service);
      return;
    }
    if (service['kind'] == 'shell') {
      await showDesktopShell(
        host: machine.id,
        hostName: machine.name,
        service: service['id'] as String? ?? 'shell',
        incarnation: service['incarnation'] as String,
      );
      return;
    }
    final generation = ++launchGeneration;
    setState(() => busy = true);
    if (service['kind'] != 'shell') {
      notice('Opening ${service['id']}…');
    }
    try {
      final publication = service['public_access'] as Map?;
      final publicUrl = publication?['url'] as String?;
      final result =
          service['kind'] == 'http' &&
              publication?['enabled'] == true &&
              publicUrl != null &&
              publicUrl.isNotEmpty
          ? {'url': publicUrl}
          : await backend.invoke('open', {
              'host': machine.id,
              'service': service['id'],
              'incarnation': service['incarnation'],
            });
      if (!mounted || !foreground || generation != launchGeneration) return;
      if (!MediaQuery.disableAnimationsOf(context)) {
        await warp.forward(from: 0);
        if (mounted) warp.reset();
      }
      if (!mounted || !foreground || generation != launchGeneration) return;
      ScaffoldMessenger.of(context).hideCurrentSnackBar();
      await backend.invoke('launch', {'url': result['url']});
    } catch (error) {
      notice('Cannot open service: ${message(error)}');
    } finally {
      if (mounted) {
        warp.reset();
        setState(() => busy = false);
        await poll();
      }
    }
  }

  Future<void> expandShell(Map<String, dynamic> shell) async {
    if (busy || !online) return;
    await showDesktopShell(
      host: shell['host'] as String,
      hostName: shell['host_name'] as String? ?? 'Shell',
      service: shell['service'] as String? ?? 'shell',
      incarnation: shell['incarnation'] as String,
      session: shell['session'] as String? ?? 'main',
    );
  }

  Future<void> confirmCloseShell(Map<String, dynamic> shell) async {
    if (busy) return;
    final host = shell['host'] as String? ?? '';
    final hostName = shell['host_name'] as String? ?? 'this device';
    final confirmed = await showWispSheet<bool>(
      context,
      title: 'Close terminal?',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Text(
            'Close all terminal sessions on $hostName? Running commands in those tabs will be terminated.',
          ),
          const SizedBox(height: 16),
          FilledButton(
            key: const ValueKey('confirm-close-shell'),
            onPressed: () => Navigator.pop(context, true),
            style: wispDangerButtonStyle(),
            child: const Text('Close sessions'),
          ),
          const SizedBox(height: 8),
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: const Text('Cancel'),
          ),
        ],
      ),
    );
    if (confirmed == true && mounted) {
      await closeShell(host);
    }
  }

  Future<void> closeShell([String? host]) async {
    if (busy) return;
    if (host != null && host.isNotEmpty) {
      desktopShells.closeHost(host, terminate: true);
    } else {
      desktopShells.closeAll(terminate: true);
    }
  }

  Future<void> serviceLinks(
    Machine machine,
    Map<String, dynamic> service,
  ) async {
    await showWispSheet<void>(
      context,
      title: 'Links for ${service['id']}',
      child: ServiceLinks(
        backend: backend,
        machine: machine,
        service: service,
        canResolve: online && machine.canOpen(service),
      ),
    );
  }

  Future<void> pair({String? code, BuildContext? from}) async {
    if (pairingSheet) return;
    pairingSheet = true;
    var requested = false;
    await showWispSheet<void>(
      from ?? context,
      title: 'Pair a device',
      child: Builder(
        builder: (pageContext) => PairingForm(
          backend: backend,
          initialCode: code,
          onPaired: () {
            if (!pageContext.mounted || !wispPageIsCurrent(pageContext)) return;
            requested = true;
            Navigator.pop(pageContext);
            unawaited(poll());
          },
        ),
      ),
    );
    pairingSheet = false;
    if (requested && mounted) {
      await poll();
      if (from == null && mounted) await manage();
    }
  }

  Future<void> manage({BuildContext? from}) async {
    var remotes = machines.where((m) => m.data['local'] != true).toList();
    var incomingPeers = <String>{};
    var pendingPeers = <String>{};
    var loading = false;
    await showWispSheet<void>(
      from ?? context,
      title: 'Paired devices',
      child: StatefulBuilder(
        builder: (pageContext, update) {
          Future<void> refreshDevices() async {
            if (loading) return;
            if (!pageContext.mounted || !wispPageIsCurrent(pageContext)) {
              return;
            }
            loading = true;
            try {
              final next = await backend.invoke('snapshot');
              final owner = await backend.invoke('ownerStatus');
              if (!pageContext.mounted || !wispPageIsCurrent(pageContext)) {
                return;
              }
              final records = Map<String, dynamic>.from(next['hosts'] as Map);
              for (final client
                  in (owner['host'] as Map?)?['clients'] as List? ?? []) {
                final id = client['peer'] as String;
                records.putIfAbsent(
                  id,
                  () => {
                    'name': client['name'],
                    'status':
                        (owner['connected_peers'] as Map? ?? {}).containsKey(id)
                        ? 'Connected'
                        : 'Paired',
                  },
                );
              }
              final nextPending = <String>{
                for (final invitation in owner['invitations'] as List? ?? [])
                  if (invitation['state'] == 'pending')
                    invitation['peer'] as String,
                for (final entry in records.entries)
                  if (entry.value['state'] == 'approval_pending') entry.key,
              };
              final newPending = nextPending
                  .difference(pendingPeers)
                  .isNotEmpty;
              pendingPeers = nextPending;
              final order = next['order'] as Map? ?? {};
              update(() {
                remotes = orderedIds(records.keys, order['machines'] as List?)
                    .map(
                      (id) => Machine(
                        id,
                        (records[id] as Map).cast<String, dynamic>(),
                      ),
                    )
                    .where((machine) => machine.data['local'] != true)
                    .toList();
                incomingPeers = {
                  for (final invitation in owner['invitations'] as List? ?? [])
                    if (invitation['state'] == 'pending')
                      invitation['peer'] as String,
                };
              });
              if (newPending) {
                WidgetsBinding.instance.addPostFrameCallback((_) {
                  if (pageContext.mounted && wispPageIsCurrent(pageContext)) {
                    Scrollable.maybeOf(pageContext)?.position.jumpTo(0);
                  }
                });
              }
            } catch (error) {
              notice('Cannot refresh paired devices: ${message(error)}');
            } finally {
              loading = false;
            }
          }

          Future<void> openPage(Future<void> page) async {
            await page;
            await refreshDevices();
          }

          return _PairedDevicesRefresh(
            onRefresh: refreshDevices,
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                ServiceSettings(
                  backend: backend,
                  android: snapshot['desktop'] != true,
                  host: snapshot['peer'] as String? ?? '',
                  inlineRequests: true,
                  onChanged: () {
                    unawaited(poll());
                    unawaited(refreshDevices());
                  },
                ),
                for (final machine in [
                  ...remotes.where((m) => m.state == 'approval_pending'),
                  ...remotes.where((m) => m.state != 'approval_pending'),
                ].where((m) => !incomingPeers.contains(m.id)))
                  Padding(
                    padding: const EdgeInsets.only(bottom: 10),
                    child: machine.state == 'approval_pending'
                        ? WispPanel(
                            key: ValueKey('outgoing-pairing-${machine.id}'),
                            child: Column(
                              crossAxisAlignment: CrossAxisAlignment.stretch,
                              children: [
                                Text(
                                  machine.name,
                                  style: const TextStyle(
                                    fontWeight: FontWeight.bold,
                                  ),
                                ),
                                const SizedBox(height: 8),
                                const Text(
                                  'Waiting for approval on the other device',
                                  style: TextStyle(color: WispColors.amber),
                                ),
                                const SizedBox(height: 8),
                                SelectableText(machine.id),
                              ],
                            ),
                          )
                        : WispPanel(
                            padding: EdgeInsets.zero,
                            child: WispListItem(
                              leading: const Icon(
                                Icons.computer,
                                color: WispColors.cyan,
                              ),
                              title: Text(
                                '${machine.name} · ${shortId(machine.id)}',
                              ),
                              titleStyle: const TextStyle(
                                fontWeight: FontWeight.bold,
                              ),
                              subtitle: Text(
                                machine.status,
                                style: TextStyle(color: stateColor(machine)),
                              ),
                              onTap: () => openPage(
                                machineSettings(machine, from: pageContext),
                              ),
                              actions: [
                                WispSettingsButton(
                                  tooltip: 'Settings for ${machine.name}',
                                  onPressed: () => openPage(
                                    machineSettings(machine, from: pageContext),
                                  ),
                                ),
                              ],
                            ),
                          ),
                  ),
                FilledButton(
                  onPressed: () {
                    openPage(pair(from: pageContext));
                  },
                  child: const Text('+ Pair another device'),
                ),
                const SizedBox(height: 8),
                OutlinedButton(
                  onPressed: () =>
                      openPage(showPairingInvitation(pageContext, backend)),
                  child: const Text('Pair this device'),
                ),
                const SizedBox(height: 16),
                SelectableText(
                  'Device ID\n${snapshot['peer'] ?? ''}',
                  style: const TextStyle(fontSize: 12, color: WispColors.muted),
                ),
              ],
            ),
          );
        },
      ),
    );
  }

  Future<void> machineSettings(Machine machine, {BuildContext? from}) async {
    if (machine.data['local'] == true) {
      await deviceSettings(servicesOnly: true, from: from);
      return;
    }
    await showWispSheet<void>(
      from ?? context,
      title: machine.name,
      child: MachineSettings(
        machine: machine,
        backend: backend,
        permissions: ServiceSettings(
          backend: backend,
          android: snapshot['desktop'] != true,
          host: snapshot['peer'] as String? ?? '',
          devicePeer: machine.id,
          showSectionSelector: false,
          onChanged: () {
            unawaited(poll());
            unawaited(refresh());
          },
        ),
        onChanged: () {
          unawaited(poll());
          unawaited(refresh());
        },
      ),
    );
  }

  Future<void> deviceSettings({
    String section = 'Devices',
    bool servicesOnly = false,
    BuildContext? from,
  }) async {
    await showWispSheet<void>(
      from ?? context,
      title: servicesOnly ? 'Local services' : section,
      child: ServiceSettings(
        android: snapshot['desktop'] != true,
        host: snapshot['peer'] as String? ?? '',
        backend: backend,
        initialSection: section,
        servicesOnly: servicesOnly,
        showSectionSelector: false,
        onChanged: () {
          unawaited(poll());
          unawaited(refresh());
        },
      ),
    );
  }

  Future<void> setServiceEnabled(
    Map<String, dynamic> service,
    bool enabled,
  ) async {
    if (busy) return;
    setState(() => busy = true);
    try {
      await backend.invoke('setServiceEnabled', {
        'service': service['id'],
        'incarnation': service['incarnation'],
        'enabled': enabled,
      });
    } catch (error) {
      notice('Cannot change service: ${message(error)}');
    } finally {
      if (mounted) {
        setState(() => busy = false);
        await poll();
      }
    }
  }

  Future<void> localServiceSettings(Map<String, dynamic> service) async {
    await showWispSheet<void>(
      context,
      title: wispServiceTypeLabel(service['kind'] as String),
      child: ServiceSettings(
        android: snapshot['desktop'] != true,
        host: snapshot['peer'] as String? ?? '',
        backend: backend,
        servicesOnly: true,
        serviceId: service['id'] as String,
        onChanged: () {
          unawaited(poll());
          unawaited(refresh());
        },
      ),
    );
    await poll();
    await refresh();
  }

  Future<void> settings() async {
    await showWispSheet<void>(
      context,
      title: 'Settings',
      child: Builder(
        builder: (pageContext) => SettingsForm(
          backend: backend,
          deviceName: snapshot['name'] as String? ?? '',
          onLocalServices: () {
            deviceSettings(servicesOnly: true, from: pageContext);
          },
          onOwnerTools: () {
            manage(from: pageContext);
          },
          onAdvancedOwnerTools: () {
            deviceSettings(section: 'Owner operations', from: pageContext);
          },
          onSaved: () {
            if (!pageContext.mounted || !wispPageIsCurrent(pageContext)) return;
            Navigator.pop(pageContext);
            notice('Settings saved');
            unawaited(poll());
          },
        ),
      ),
    );
  }

  Future<void> reorder(
    String group,
    List<String> ids,
    String moved,
    String target,
  ) async {
    final next = [...ids];
    final destination = next.indexOf(target);
    if (!next.remove(moved) || destination < 0) return;
    next.insert(destination, moved);
    setState(() => order[group] = next);
    try {
      await backend.invoke('saveOrder', {'group': group, 'ids': next});
      await poll();
    } catch (error) {
      notice(message(error));
      await poll();
    }
  }

  Color stateColor(Machine m) => m.online
      ? WispColors.green
      : m.state == 'approval_pending'
      ? WispColors.amber
      : m.state == 'host_denied'
      ? WispColors.error
      : WispColors.muted;
  String shortId(String id) => id.length > 8 ? id.substring(0, 8) : id;
  String telemetry(Machine machine) {
    final peer = (snapshot['telemetry'] as Map?)?['peers']?[machine.id] as Map?;
    if (peer == null) return '';
    final path = peer['path'] == 'direct'
        ? 'Direct'
        : peer['path'] == 'relay'
        ? 'Relay'
        : '';
    final rtt = peer['rtt_ms'] as num? ?? 0;
    return [
      if (path.isNotEmpty) path,
      if (rtt > 0) '${rtt.round()}ms',
    ].join(' · ');
  }

  Widget directoryCard(Machine machine, {bool local = false}) => MachineCard(
    machine: machine,
    showHeader: !local,
    order: order,
    telemetry: local ? '' : telemetry(machine),
    stateColor: stateColor(machine),
    busy: busy,
    onManage: () => machineSettings(machine),
    onOpen: (service) => open(machine, service),
    onLinks: (service) => serviceLinks(machine, service),
    onSSHStatus: snapshot['desktop'] == true
        ? () => backend.invoke('sshStatus', {'host': machine.id})
        : null,
    onSSHSetup: snapshot['desktop'] == true
        ? () async {
            final result = await backend.invoke('sshSetup', {
              'host': machine.id,
            });
            return result['command'] as String;
          }
        : null,
    onSettings: local ? localServiceSettings : null,
    onToggle: local ? setServiceEnabled : null,
    onReorder: reorder,
    onFilesDropped: snapshot['desktop'] == true && !local && online
        ? (service, paths) => showFiles(machine, service, paths: paths)
        : null,
  );

  Future<void> receiveSharedFiles(List<Map<String, dynamic>> files) async {
    try {
      if (!online) {
        await backend.invoke('setAccess', {'enabled': true});
      }
      await refresh();
      await poll();
      if (!mounted) return;
      final receivers = machines
          .where(
            (machine) =>
                machine.data['local'] != true &&
                machine.online &&
                machine.services.any(
                  (service) =>
                      isInboxService(service) && machine.canOpen(service),
                ),
          )
          .toList();
      final receiver = await showWispSheet<Machine>(
        context,
        title:
            'Send ${files.length == 1 ? 'file' : '${files.length} files'} to…',
        child: Material(
          type: MaterialType.transparency,
          child: Column(
            mainAxisSize: MainAxisSize.min,
            children: [
              for (final file in files)
                WispListItem(
                  leading: const Icon(Icons.insert_drive_file_outlined),
                  title: Text(file['name'] as String),
                ),
              const Divider(),
              if (receivers.isEmpty)
                const Text(
                  'No devices are ready to receive files. Go online on a paired device and enable its File inbox.',
                ),
              for (final machine in receivers)
                WispListItem(
                  key: ValueKey('share-receiver-${machine.id}'),
                  leading: const Icon(Icons.computer, color: WispColors.cyan),
                  title: Text(machine.name),
                  onTap: () => Navigator.pop(context, machine),
                ),
            ],
          ),
        ),
      );
      if (receiver == null || !mounted) return;
      final service = await chooseInbox(receiver);
      if (service == null || !mounted) return;
      await showFiles(receiver, service, files: files);
    } catch (error) {
      if (mounted) notice('Cannot share files: ${message(error)}');
    } finally {
      sharing = false;
    }
  }

  Future<void> dropFiles(List<String> paths) async {
    final receivers = machines
        .where(
          (machine) =>
              machine.data['local'] != true &&
              machine.online &&
              machine.services.any(
                (service) =>
                    isInboxService(service) && machine.canOpen(service),
              ),
        )
        .toList();
    if (receivers.isEmpty) {
      notice(
        'No devices are ready to receive files. Ask a paired device to grant Files access.',
      );
      return;
    }
    final receiver = receivers.length == 1
        ? receivers.single
        : await showWispSheet<Machine>(
            context,
            title: 'Send files to…',
            child: Column(
              mainAxisSize: MainAxisSize.min,
              children: [
                for (final machine in receivers)
                  WispListItem(
                    leading: const Icon(Icons.computer, color: WispColors.cyan),
                    title: Text(machine.name),
                    onTap: () => Navigator.pop(context, machine),
                  ),
              ],
            ),
          );
    if (receiver == null || !mounted) return;
    final service = await chooseInbox(receiver);
    if (service == null || !mounted) return;
    await showFiles(receiver, service, paths: paths);
  }

  Future<Map<String, dynamic>?> chooseInbox(Machine machine) async {
    final inboxes = machine.services
        .where((service) => isInboxService(service) && machine.canOpen(service))
        .toList();
    if (inboxes.length == 1) return inboxes.single;
    if (inboxes.isEmpty) return null;
    return showWispSheet<Map<String, dynamic>>(
      context,
      title: 'Choose inbox on ${machine.name}',
      child: Column(
        children: [
          for (final service in inboxes)
            WispListItem(
              key: ValueKey('choose-inbox-${service['id']}'),
              leading: const Icon(
                Icons.move_to_inbox_outlined,
                color: WispColors.cyan,
              ),
              title: Text(service['id'] as String),
              subtitle: const Text('File inbox'),
              onTap: () => Navigator.pop(context, service),
            ),
        ],
      ),
    );
  }

  Future<void> showTransfers({String? host, String? hostName}) =>
      showWispSheet<void>(
        context,
        title: hostName == null ? 'File transfers' : 'Uploads to $hostName',
        child: FileTransferList(transfers: fileTransfers, host: host),
      );

  Future<void> confirmCancelUploads(String host, String hostName) async {
    final active = fileTransfers.jobs.where(
      (job) => job.machine.id == host && !job.finished,
    );
    if (active.isEmpty) return;
    final confirmed = await showWispSheet<bool>(
      context,
      title: 'Cancel uploads to $hostName?',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          const Text(
            'Files already sent are kept. Cancelled uploads can be resumed from Files.',
          ),
          const SizedBox(height: 16),
          FilledButton(
            key: const ValueKey('confirm-cancel-uploads'),
            onPressed: () => Navigator.pop(context, true),
            child: const Text('Cancel uploads'),
          ),
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: const Text('Keep uploading'),
          ),
        ],
      ),
    );
    if (!mounted || confirmed != true) return;
    for (final job in active.toList()) {
      fileTransfers.cancel(job);
    }
  }

  Widget uploadDocks() {
    final groups = <String, List<DesktopFileTransfer>>{};
    if (online) {
      for (final job in fileTransfers.jobs.where((job) => !job.finished)) {
        groups.putIfAbsent(job.machine.id, () => []).add(job);
      }
    }
    if (groups.isEmpty) return const SizedBox.shrink();
    return Padding(
      padding: const EdgeInsets.only(top: 4, bottom: 8),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          for (final entry in groups.entries.indexed) ...[
            if (entry.$1 > 0) const SizedBox(height: 6),
            _buildUploadDock(
              entry.$2.key,
              entry.$2.value,
              primary: entry.$1 == 0,
            ),
          ],
        ],
      ),
    );
  }

  Future<void> closeDownloads() async {
    final active = sharedDownloads.jobs.where((job) => !job.finished).toList();
    if (active.isNotEmpty) {
      final confirmed = await showWispSheet<bool>(
        context,
        title: 'Cancel downloads?',
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            const Text(
              'Completed files are kept. Cancelled downloads can be resumed from their share.',
            ),
            const SizedBox(height: 16),
            FilledButton(
              onPressed: () => Navigator.pop(context, true),
              child: const Text('Cancel downloads'),
            ),
            TextButton(
              onPressed: () => Navigator.pop(context, false),
              child: const Text('Keep downloading'),
            ),
          ],
        ),
      );
      if (confirmed != true || !mounted) return;
      await Future.wait(active.map(sharedDownloads.cancel));
    } else {
      sharedDownloads.clearFinished();
    }
  }

  Widget downloadDock() {
    if (sharedDownloads.jobs.isEmpty) return const SizedBox.shrink();
    final active = sharedDownloads.jobs.where((job) => !job.finished).length;
    return Padding(
      padding: const EdgeInsets.only(top: 4, bottom: 8),
      child: WispDockBar(
        key: const ValueKey('share-downloads-dock'),
        title: active > 0
            ? 'Downloads · $active active'
            : 'Downloads · ${sharedDownloads.jobs.length} finished',
        statusColor: active > 0 ? WispColors.green : WispColors.cyan,
        expandKey: const ValueKey('share-downloads-expand'),
        closeKey: const ValueKey('share-downloads-close'),
        expandLabel: 'View downloads',
        closeLabel: active > 0
            ? 'Cancel downloads'
            : 'Dismiss finished downloads',
        onExpand: () => showWispSheet<void>(
          context,
          title: 'Downloads',
          child: SharedDownloadsList(downloads: sharedDownloads),
        ),
        onClose: closeDownloads,
      ),
    );
  }

  Widget _buildUploadDock(
    String host,
    List<DesktopFileTransfer> jobs, {
    required bool primary,
  }) {
    final hostName = jobs.first.hostName;
    final label = jobs.length == 1
        ? 'Uploading file'
        : 'Uploading ${jobs.length} files';
    return WispDockBar(
      key: ValueKey(
        primary ? 'file-transfers-dock' : 'file-transfers-dock-$host',
      ),
      title: '$hostName · $label',
      statusColor: jobs.any((job) => job.uploading)
          ? WispColors.green
          : WispColors.amber,
      expandKey: ValueKey('file-dock-expand-$host'),
      closeKey: ValueKey('file-dock-close-$host'),
      expandLabel: 'Expand uploads to $hostName',
      closeLabel: 'Cancel uploads to $hostName',
      onExpand: busy
          ? null
          : () => showTransfers(host: host, hostName: hostName),
      onClose: busy ? null : () => confirmCancelUploads(host, hostName),
    );
  }

  Widget directory() {
    final local = machines
        .where((m) => m.data['local'] == true && m.services.isNotEmpty)
        .toList();
    final remote = machines.where((m) => m.data['local'] != true).toList();
    final pendingRequests = snapshot['pendingRequests'] as int? ?? 0;
    final bottomPadding = 24 + MediaQuery.viewPaddingOf(context).bottom;
    if (local.isEmpty && remote.isEmpty) {
      return RefreshIndicator(
        key: const ValueKey('directory'),
        onRefresh: refresh,
        child: LayoutBuilder(
          builder: (context, constraints) => SingleChildScrollView(
            physics: const AlwaysScrollableScrollPhysics(),
            padding: EdgeInsets.only(bottom: bottomPadding),
            child: ConstrainedBox(
              constraints: BoxConstraints(
                minHeight: math.max(0.0, constraints.maxHeight - bottomPadding),
              ),
              child: Center(
                child: ConstrainedBox(
                  constraints: const BoxConstraints(maxWidth: 420),
                  child: WispPanel(
                    key: const ValueKey('empty-directory'),
                    color: const Color(0xd80b1528),
                    radius: 24,
                    padding: const EdgeInsets.all(24),
                    child: Column(
                      mainAxisSize: MainAxisSize.min,
                      crossAxisAlignment: CrossAxisAlignment.stretch,
                      children: [
                        Center(
                          child: Container(
                            width: 52,
                            height: 52,
                            decoration: BoxDecoration(
                              color: const Color(0xff132548),
                              shape: BoxShape.circle,
                              border: Border.all(
                                color: const Color(0x664b73c2),
                              ),
                            ),
                            child: const Icon(
                              Icons.hub_outlined,
                              color: WispColors.cyan,
                              size: 26,
                            ),
                          ),
                        ),
                        const SizedBox(height: 16),
                        const Text(
                          'Ready to connect',
                          textAlign: TextAlign.center,
                          style: TextStyle(
                            fontSize: 20,
                            fontWeight: FontWeight.bold,
                            letterSpacing: .3,
                          ),
                        ),
                        const SizedBox(height: 8),
                        const Text(
                          'Pair another Wispnet device to open its remote services, or share a local service from Settings.',
                          textAlign: TextAlign.center,
                          style: TextStyle(
                            color: WispColors.muted,
                            height: 1.45,
                          ),
                        ),
                        const SizedBox(height: 20),
                        if (pendingRequests > 0) ...[
                          OutlinedButton.icon(
                            onPressed: busy ? null : manage,
                            icon: const Icon(
                              Icons.notifications_active_outlined,
                              size: 18,
                            ),
                            label: Text(
                              'Review $pendingRequests pairing ${pendingRequests == 1 ? 'request' : 'requests'}',
                            ),
                          ),
                          const SizedBox(height: 10),
                        ],
                        FilledButton.icon(
                          key: const ValueKey('empty-pair-button'),
                          onPressed: busy ? null : pair,
                          icon: const Icon(Icons.qr_code_scanner, size: 20),
                          label: const Text('Pair a device'),
                        ),
                        const SizedBox(height: 6),
                        TextButton(
                          key: const ValueKey('empty-local-services-button'),
                          onPressed: busy
                              ? null
                              : () => deviceSettings(servicesOnly: true),
                          child: const Text('Configure local services ›'),
                        ),
                      ],
                    ),
                  ),
                ),
              ),
            ),
          ),
        ),
      );
    }
    return RefreshIndicator(
      key: const ValueKey('directory'),
      onRefresh: refresh,
      child: ListView(
        padding: EdgeInsets.only(bottom: bottomPadding),
        physics: const AlwaysScrollableScrollPhysics(),
        children: [
          if (local.isNotEmpty)
            Padding(
              padding: const EdgeInsets.only(bottom: 24),
              child: Column(
                key: const ValueKey('local-sector'),
                crossAxisAlignment: CrossAxisAlignment.stretch,
                children: [
                  const WispSectionHeading(title: 'LOCAL SERVICES'),
                  const SizedBox(height: 12),
                  for (final machine in local)
                    directoryCard(machine, local: true),
                ],
              ),
            ),
          Column(
            key: const ValueKey('remote-sector'),
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              WispSectionHeading(
                title: 'REMOTE SERVICES',
                trailing: remote.isEmpty
                    ? null
                    : IconButton(
                        tooltip: 'Refresh remote services',
                        onPressed: refreshing ? null : refresh,
                        icon: refreshing
                            ? const SizedBox(
                                width: 18,
                                height: 18,
                                child: CircularProgressIndicator(
                                  strokeWidth: 2,
                                ),
                              )
                            : const Icon(Icons.refresh),
                      ),
              ),
              const SizedBox(height: 12),
              if (remote.isEmpty)
                WispPanel(
                  key: const ValueKey('empty-remote-card'),
                  color: const Color(0xd80b1528),
                  padding: const EdgeInsets.all(20),
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      const Text(
                        'No paired devices yet',
                        style: TextStyle(
                          fontSize: 15,
                          fontWeight: FontWeight.bold,
                        ),
                      ),
                      const SizedBox(height: 6),
                      const Text(
                        'Pair another Wispnet device to open its shared services here.',
                        style: TextStyle(color: WispColors.muted, height: 1.4),
                      ),
                      const SizedBox(height: 14),
                      FilledButton.icon(
                        key: const ValueKey('remote-pair-button'),
                        onPressed: busy ? null : pair,
                        icon: const Icon(Icons.qr_code_scanner, size: 18),
                        label: const Text('Pair a device'),
                      ),
                    ],
                  ),
                ),
              for (final machine in remote)
                Padding(
                  padding: const EdgeInsets.only(bottom: 14),
                  child: ReorderTile(
                    group: 'machines',
                    id: machine.id,
                    onDrop: (moved) => reorder(
                      'machines',
                      remote.map((m) => m.id).toList(),
                      moved,
                      machine.id,
                    ),
                    child: directoryCard(machine),
                  ),
                ),
            ],
          ),
        ],
      ),
    );
  }

  Widget shellDock() {
    final shells = minimizedShells;
    final animate = !MediaQuery.disableAnimationsOf(context);
    return AnimatedSwitcher(
      duration: animate ? const Duration(milliseconds: 220) : Duration.zero,
      switchInCurve: Curves.easeOutCubic,
      switchOutCurve: Curves.easeInCubic,
      transitionBuilder: (child, animation) => FadeTransition(
        opacity: animation,
        child: SlideTransition(
          position: Tween<Offset>(
            begin: const Offset(0, .35),
            end: Offset.zero,
          ).animate(animation),
          child: child,
        ),
      ),
      child: shells.isEmpty
          ? const SizedBox.shrink()
          : Padding(
              key: ValueKey(
                'shell-docks-${shells.map((s) => s['host']).join(',')}',
              ),
              padding: const EdgeInsets.only(top: 4, bottom: 8),
              child: Column(
                mainAxisSize: MainAxisSize.min,
                children: [
                  for (var i = 0; i < shells.length; i++) ...[
                    if (i > 0) const SizedBox(height: 6),
                    _buildDockBar(shells[i], primary: i == 0),
                  ],
                ],
              ),
            ),
    );
  }

  Widget _buildDockBar(Map<String, dynamic> shell, {required bool primary}) {
    final host = shell['host'] as String? ?? '';
    final title = (shell['title'] as String?)?.isNotEmpty == true
        ? shell['title'] as String
        : '${shell['host_name'] ?? 'Shell'} · ${shell['session'] ?? 'main'}';
    return WispDockBar(
      key: ValueKey(primary ? 'shell-dock' : 'shell-dock-$host'),
      title: title,
      statusColor: shell['state'] == 'attached'
          ? WispColors.green
          : shell['state'] == 'connecting'
          ? WispColors.amber
          : WispColors.error,
      expandKey: ValueKey(
        primary ? 'shell-dock-expand' : 'shell-dock-expand-$host',
      ),
      closeKey: ValueKey(
        primary ? 'shell-dock-close' : 'shell-dock-close-$host',
      ),
      expandLabel: 'Expand shell $title',
      closeLabel: 'Close shell',
      onExpand: busy ? null : () => expandShell(shell),
      onClose: busy ? null : () => confirmCloseShell(shell),
    );
  }

  @override
  Widget build(BuildContext context) {
    final list = machines;
    final scaffold = Scaffold(
      body: WispSpace(
        online: online,
        warp: warp,
        child: IgnorePointer(
          ignoring: entering,
          child: FadeTransition(
            key: const ValueKey('entry-reveal'),
            opacity: entering
                ? online
                      ? warp.drive(CurveTween(curve: const Interval(7 / 9, 1)))
                      : const AlwaysStoppedAnimation(0.0)
                : const AlwaysStoppedAnimation(1.0),
            child: SafeArea(
              bottom: false,
              child: Center(
                child: ConstrainedBox(
                  constraints: const BoxConstraints(maxWidth: 1200),
                  child: Padding(
                    padding: const EdgeInsets.symmetric(horizontal: 20),
                    child: Column(
                      children: [
                        if (online)
                          WispHeader(
                            label:
                                '${snapshot['name'] ?? 'Wispnet'}${snapshot['peer'] == null ? '' : ' · ${shortId(snapshot['peer'] as String)}'}',
                            online: online,
                            onSettings: snapshot.isEmpty
                                ? null
                                : () => settings(),
                            onToggle: busy ? null : toggle,
                          ),
                        Expanded(
                          child: initializationError != null
                              ? Center(
                                  child: Column(
                                    mainAxisSize: MainAxisSize.min,
                                    children: [
                                      Text(
                                        initializationError!,
                                        textAlign: TextAlign.center,
                                      ),
                                      const SizedBox(height: 16),
                                      FilledButton(
                                        onPressed: initialize,
                                        child: const Text('Retry'),
                                      ),
                                    ],
                                  ),
                                )
                              : snapshot.isEmpty
                              ? const Center(child: CircularProgressIndicator())
                              : AnimatedSwitcher(
                                  duration:
                                      MediaQuery.disableAnimationsOf(context)
                                      ? Duration.zero
                                      : const Duration(milliseconds: 220),
                                  child: online
                                      ? directory()
                                      : Center(
                                          key: const ValueKey('standby'),
                                          child: Column(
                                            mainAxisSize: MainAxisSize.min,
                                            children: [
                                              SpaceToggle(
                                                first: list.isEmpty,
                                                busy: busy,
                                                beacons: list
                                                    .map(stateColor)
                                                    .toList(),
                                                onPressed: busy
                                                    ? null
                                                    : list.isEmpty
                                                    ? pair
                                                    : toggle,
                                              ),
                                              const SizedBox(height: 8),
                                              Text(
                                                list.isEmpty
                                                    ? 'establish your wispnet'
                                                    : 'hyperjump to wispnet',
                                                textAlign: TextAlign.center,
                                                style: const TextStyle(
                                                  fontSize: 21,
                                                  letterSpacing: 1,
                                                  shadows: [
                                                    Shadow(
                                                      color: Color(0x883868c8),
                                                      blurRadius: 10,
                                                      offset: Offset(0, 2),
                                                    ),
                                                  ],
                                                ),
                                              ),
                                            ],
                                          ),
                                        ),
                                ),
                        ),
                        shellDock(),
                        uploadDocks(),
                        downloadDock(),
                        if (minimizedShells.isNotEmpty ||
                            fileTransfers.jobs.any((job) => !job.finished) ||
                            sharedDownloads.jobs.isNotEmpty)
                          SizedBox(
                            height: MediaQuery.viewPaddingOf(context).bottom,
                          ),
                      ],
                    ),
                  ),
                ),
              ),
            ),
          ),
        ),
      ),
    );
    return snapshot['desktop'] == true
        ? FileDropArea(
            enabled: online && !busy && !pairingSheet,
            onFiles: dropFiles,
            child: scaffold,
          )
        : scaffold;
  }
}

class _PairedDevicesRefresh extends StatefulWidget {
  const _PairedDevicesRefresh({required this.onRefresh, required this.child});
  final Future<void> Function() onRefresh;
  final Widget child;
  @override
  State<_PairedDevicesRefresh> createState() => _PairedDevicesRefreshState();
}

class _PairedDevicesRefreshState extends State<_PairedDevicesRefresh> {
  Timer? timer;
  @override
  void initState() {
    super.initState();
    scheduleMicrotask(() {
      if (mounted) unawaited(widget.onRefresh());
    });
    timer = Timer.periodic(
      const Duration(seconds: 3),
      (_) => unawaited(widget.onRefresh()),
    );
  }

  @override
  void dispose() {
    timer?.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => widget.child;
}
