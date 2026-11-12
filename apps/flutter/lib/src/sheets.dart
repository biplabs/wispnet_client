import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'backend.dart';
import 'pairing_qr.dart';
import 'qr_camera_sheet.dart';
import '../ui.dart';

String failureMessage(Object error) {
  final message = error is FormatException
      ? error.message
      : error is PlatformException
      ? error.message ?? error.code
      : error.toString();
  if (error is PlatformException && error.code == 'invitation_denied') {
    return 'This host did not accept the pairing code. Generate a new code on the other device.';
  }
  if (message.toLowerCase().contains('expired')) {
    return 'Pairing code expired or invalid. Generate a new code on the other device.';
  }
  return message;
}

class PairingForm extends StatefulWidget {
  const PairingForm({
    super.key,
    required this.backend,
    required this.onPaired,
    this.initialCode,
  });
  final WispnetBackend backend;
  final VoidCallback onPaired;
  final String? initialCode;
  @override
  State<PairingForm> createState() => _PairingFormState();
}

class _PairingFormState extends State<PairingForm> {
  late final TextEditingController code = TextEditingController(
    text: widget.initialCode,
  )..addListener(changed);
  bool busy = false, error = false;
  late int tab = widget.initialCode == null ? 0 : 1;
  bool camera = false;
  String feedback = '';
  Map<String, dynamic>? preview;
  @override
  void initState() {
    super.initState();
    loadQrCapabilities();
    if (widget.initialCode != null) {
      WidgetsBinding.instance.addPostFrameCallback((_) => review());
    }
  }

  Future<void> loadQrCapabilities() async {
    try {
      final capabilities = await widget.backend.invoke('qrCapabilities');
      if (mounted) {
        setState(() {
          camera = capabilities['camera'] == true;
        });
      }
    } catch (_) {
      // Image import and pairing-code input remain available without a camera.
    }
  }

  @override
  void dispose() {
    code.dispose();
    super.dispose();
  }

  void changed() {
    setState(() {
      preview = null;
      feedback = '';
    });
  }

  String normalized() {
    final value = code.text.replaceAll(RegExp(r'\s+'), '');
    if (!value.startsWith('wispnet1:') || utf8.encode(value).length > 8192) {
      throw const FormatException(
        'Paste the complete Wispnet pairing code or scan its QR.',
      );
    }
    return value;
  }

  Future<void> review() async {
    if (busy) return;
    setState(() {
      busy = true;
      error = false;
      feedback = 'Checking pairing code…';
    });
    try {
      final result = await widget.backend.invoke('preview', {
        'code': normalized(),
      });
      if (mounted) {
        setState(() {
          preview = result;
          feedback = 'Pairing code ready. Request pairing, then approve it on the other device.';
        });
      }
    } catch (e) {
      if (mounted) {
        setState(() {
          error = true;
          feedback = failureMessage(e);
        });
      }
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  Future<void> pair() async {
    if (busy) return;
    FocusManager.instance.primaryFocus?.unfocus();
    setState(() {
      busy = true;
      error = false;
      feedback = 'Requesting pairing… This may take up to a minute.';
    });
    try {
      await widget.backend.invoke('pair', {'code': normalized()});
      if (mounted && wispPageIsCurrent(context)) widget.onPaired();
    } catch (e) {
      if (mounted) {
        setState(() {
          error = true;
          feedback = failureMessage(e);
        });
      }
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  Future<void> qr(String method) async {
    if (busy) return;
    setState(() {
      busy = true;
      error = false;
      feedback = method == 'scanQr'
          ? 'Opening camera…'
          : 'Choose a pairing QR image…';
    });
    try {
      final result = method == 'scanQr'
          ? {
              'code': await showWispSheet<String>(
                context,
                title: 'Scan pairing QR',
                child: QrCameraSheet(backend: widget.backend),
              ),
            }
          : await widget.backend.invoke(method);
      if (!mounted || !wispPageIsCurrent(context)) return;
      final scanned = result['code'] as String?;
      if (scanned == null) {
        setState(() {
          busy = false;
          feedback =
              'Scan cancelled. Import a QR image or paste a pairing code.';
        });
        return;
      }
      code.text = normalizedPairingCode(scanned);
      setState(() {
        busy = false;
        tab = 1;
      });
      await pair();
    } catch (e) {
      if (mounted) {
        setState(() {
          busy = false;
          error = true;
          feedback = failureMessage(e);
        });
      }
    }
  }

  @override
  Widget build(BuildContext context) => PopScope(
    canPop: !busy,
    child: Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        ...[
          WispPanel(
            radius: 14,
            color: const Color(0xff0d1526),
            padding: const EdgeInsets.all(4),
            child: Row(
              children: [
                for (final entry in {0: 'QR', 1: 'Pairing Code'}.entries)
                  Expanded(
                    child: TextButton(
                      onPressed: busy
                          ? null
                          : () => setState(() => tab = entry.key),
                      style: TextButton.styleFrom(
                        foregroundColor: tab == entry.key
                            ? WispColors.text
                            : WispColors.muted,
                        backgroundColor: tab == entry.key
                            ? const Color(0xff1f3666)
                            : Colors.transparent,
                        minimumSize: const Size(0, 44),
                        shape: RoundedRectangleBorder(
                          borderRadius: BorderRadius.circular(10),
                        ),
                      ),
                      child: Text(entry.value),
                    ),
                  ),
              ],
            ),
          ),
          const SizedBox(height: 14),
        ],
        if (tab == 0) ...[
          if (camera) ...[
            FilledButton(
              onPressed: busy ? null : () => qr('scanQr'),
              child: const Text('Scan with camera'),
            ),
            const SizedBox(height: 10),
          ],
          OutlinedButton(
            onPressed: busy ? null : () => qr('importQr'),
            child: const Text('Import QR image'),
          ),
        ] else ...[
          const Text(
            'Paste the pairing code from the other device.',
            style: TextStyle(color: WispColors.muted),
          ),
          const SizedBox(height: 12),
          TextField(
            controller: code,
            enabled: !busy,
            minLines: 4,
            maxLines: 6,
            autocorrect: false,
            enableSuggestions: false,
            decoration: const InputDecoration(
              hintText: 'Pairing code',
              labelText: 'Pairing code',
            ),
          ),
          const SizedBox(height: 10),
          FilledButton(
            onPressed: busy || code.text.trim().isEmpty ? null : pair,
            child: Text(busy ? 'Pairing…' : 'Pair'),
          ),
        ],
        if (feedback.isNotEmpty)
          Padding(
            padding: const EdgeInsets.symmetric(vertical: 12),
            child: Semantics(
              liveRegion: true,
              child: Text(
                feedback,
                style: TextStyle(
                  color: error ? WispColors.error : const Color(0xffb8d4ff),
                ),
              ),
            ),
          ),
        if (preview != null)
          WispPanel(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                const Text('Pair with this device'),
                const SizedBox(height: 8),
                Text(
                  preview!['host_name'] as String? ?? 'Device',
                  style: const TextStyle(
                    fontSize: 16,
                    fontWeight: FontWeight.bold,
                  ),
                ),
                const SizedBox(height: 8),
                SelectableText(
                  preview!['host'] as String,
                  style: const TextStyle(fontSize: 12, color: WispColors.muted),
                ),
                const SizedBox(height: 12),
                FilledButton(
                  onPressed: busy ? null : pair,
                  child: const Text('Request pairing'),
                ),
              ],
            ),
          ),
      ],
    ),
  );
}

class MachineSettings extends StatefulWidget {
  const MachineSettings({
    super.key,
    required this.machine,
    required this.backend,
    required this.onChanged,
  });
  final Machine machine;
  final WispnetBackend backend;
  final VoidCallback onChanged;
  @override
  State<MachineSettings> createState() => _MachineSettingsState();
}

class _MachineSettingsState extends State<MachineSettings> {
  bool busy = false;
  String? error;
  Future<void> perform(String method) async {
    setState(() {
      busy = true;
      error = null;
    });
    try {
      await widget.backend.invoke(method, {'host': widget.machine.id});
      widget.onChanged();
      if (mounted && wispPageIsCurrent(context)) Navigator.pop(context);
    } catch (e) {
      if (mounted) setState(() => error = failureMessage(e));
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  ButtonStyle get dangerStyle => wispDangerButtonStyle();

  Future<void> confirmRemove() async {
    final confirmed = await showWispSheet<bool>(
      context,
      title: 'Remove device?',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Text(
            'Remove ${widget.machine.name} and close its Wispnet access on this device? This device will forget the pairing and close its local service routes. The other device will learn about removal on its next contact. You can pair again later.',
          ),
          const SizedBox(height: 16),
          FilledButton(
            style: dangerStyle,
            onPressed: () => Navigator.pop(context, true),
            child: const Text('Remove device'),
          ),
          const SizedBox(height: 12),
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: const Text('Cancel'),
          ),
        ],
      ),
    );
    if (confirmed == true && mounted) await perform('removeHost');
  }

  @override
  Widget build(BuildContext context) => Column(
    crossAxisAlignment: CrossAxisAlignment.stretch,
    children: [
      SelectableText(
        'Device name\n${widget.machine.name}\n\nDevice ID\n${widget.machine.id}',
        style: const TextStyle(color: WispColors.muted),
      ),
      const SizedBox(height: 16),
      OutlinedButton(
        style: dangerStyle,
        onPressed: busy ? null : confirmRemove,
        child: const Text('Remove'),
      ),
      if (error != null)
        Padding(
          padding: const EdgeInsets.only(top: 12),
          child: Text(error!, style: const TextStyle(color: WispColors.error)),
        ),
    ],
  );
}

String diagnosticObserved(Object? value) => value == null
    ? '?'
    : value == true
    ? '✓'
    : '✗';

void appendDiagnosticObservation(
  List<String> lines,
  String label,
  Map side, {
  Map? routerMapping,
}) {
  final network = side['network'] as Map?;
  final n = network?['report'] as Map?;
  final p = side['paths'] as Map?;
  lines.add('\n$label');
  if (side.containsKey('relay_only')) {
    lines.add(
      '• ${side['relay_only'] == true ? "✗" : "✓"} Direct UDP transports enabled',
    );
  }
  final mapping = routerMapping;
  if (mapping == null) {
    lines.add('• ? Automatic router mapping status unavailable');
  } else {
    lines.add(
      '• ${diagnosticObserved(mapping['enabled'])} Automatic router mapping enabled',
    );
    lines.add(
      '• Router responses: UPnP ${diagnosticObserved(mapping['upnp'])} · PCP ${diagnosticObserved(mapping['pcp'])} · NAT-PMP ${diagnosticObserved(mapping['nat_pmp'])}',
    );
    lines.add(
      '• ${diagnosticObserved(mapping['active'])} Router mapping granted${mapping['active'] == true ? " · UDP ${mapping['external_port']}" : ""}',
    );
    if (mapping['active'] == true) {
      lines.add(
        '• ${diagnosticObserved(mapping['public_address'])} Mapped address is public',
      );
      lines.add(
        '• A granted mapping does not prove reachability; see the direct-path result.',
      );
    }
  }
  if (p != null) {
    lines.add(
      '• ${p['initial_path']} → ${p['selected_path']} · RTT: ${p['rtt_ms'] ?? "unavailable"} ms',
    );
    lines.add(
      '• Observed for ${side['elapsed_ms']} ms · relay: ${side['relay_status']}',
    );
  }
  lines.add(
    network?['fresh'] == true
        ? '• Discovery: network report updated during this test (${network?['wait_ms']} ms)'
        : n != null
        ? '• Discovery: cached; no new report observed during this test'
        : '• Discovery: unavailable during this test',
  );
  for (final check in <(String, Object?)>[
    ('UDP response IPv4', n?['udp_v4']),
    ('UDP response IPv6', n?['udp_v6']),
    ('Public IPv4 address discovered', n?['public_ipv4_discovered']),
    ('Public IPv6 address discovered', n?['public_ipv6_discovered']),
    ('NAT mapping varies by destination', n?['mapping_varies']),
    ('Captive portal suspected (heuristic)', n?['captive_portal']),
  ]) {
    lines.add('• ${diagnosticObserved(check.$2)} ${check.$1}');
  }
  if (n != null &&
      (n['udp_v4'] == true || n['udp_v6'] == true) &&
      n['public_ipv4_discovered'] == false &&
      n['public_ipv6_discovered'] == false) {
    lines.add(
      '• Discovery responded, but no public IP was found. A discovery server behind the same router can return a private LAN address.',
    );
  }
  if (n?['mapping_varies'] == true) {
    lines.add(
      '• Destination-dependent NAT can make direct connections harder.',
    );
  }
  if (n != null && n['udp_v4'] == false && n['udp_v6'] == false) {
    lines.add(
      '• No UDP discovery response in this report. This alone does not prove UDP is blocked.',
    );
  }
  if (side.containsKey('peer_ip_candidates')) {
    lines.add(
      '• Known peer IP addresses in transport cache: ${side['peer_ip_candidates'] ?? "unavailable"}',
    );
  }
  lines.add(
    '• Hole-punch candidate list: unavailable in the transport API; cache count does not measure address exchange.',
  );
  if (p != null) {
    lines.add(
      '• Direct paths seen: ${p['direct_paths_seen']} · closed: ${p['direct_paths_closed']}',
    );
    lines.add(
      '• Datagrams on direct paths: sent ${p['direct_datagrams_sent']} · received ${p['direct_datagrams_received']}',
    );
    lines.add(
      '• Validation on direct paths: challenges sent ${p['direct_challenges_sent']} · responses received ${p['direct_responses_received']}',
    );
    lines.add(
      '• Validation across all paths (includes relay): challenges sent ${p['connection_challenges_sent']} · responses received ${p['connection_responses_received']}',
    );
    if ((p['events_lost'] as num? ?? 0) > 0) {
      lines.add(
        '• Some path events were lost; activity counts may be incomplete.',
      );
    }
  }
}

class ConnectionDiagnostics extends StatefulWidget {
  const ConnectionDiagnostics({super.key, required this.backend});
  final WispnetBackend backend;
  @override
  State<ConnectionDiagnostics> createState() => _ConnectionDiagnosticsState();
}

class _ConnectionDiagnosticsState extends State<ConnectionDiagnostics> {
  bool running = false;
  String report = '';
  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) run();
    });
  }

  Future<void> run() async {
    setState(() {
      running = true;
      report = 'Refreshing discovery and testing paired devices…';
    });
    final lines = <String>[
      'Connection diagnostic · ${DateTime.now().toLocal().toIso8601String().split(".").first.replaceFirst("T", " ")}',
      '✓ Yes · ✗ No · ? Unavailable',
    ];
    try {
      final snapshot = await widget.backend.invoke('snapshot');
      final hosts = snapshot['hosts'] as Map? ?? {};
      final remoteHosts = hosts.entries.where(
        (entry) =>
            entry.key != snapshot['peer'] &&
            (entry.value as Map)['local'] != true,
      );
      if (remoteHosts.isEmpty) lines.add('No paired devices to test.');
      for (final entry in remoteHosts) {
        final host = entry.key;
        if (!mounted) return;
        setState(
          () => report =
              '${lines.join('\n')}\nTesting ${hosts[host]['name']}… (up to 55 seconds)',
        );
        Map<String, dynamic> result;
        try {
          result = await widget.backend.invoke('connectionDiagnostics', {
            'host': host,
          });
        } catch (error) {
          lines.add('\n${entry.value['name']}');
          lines.add(
            '• ? Could not verify connection: ${failureMessage(error)}',
          );
          continue;
        }
        lines.add(
          '• Network type: ${result['network'] ?? "Unavailable"} · VPN: ${result['vpn'] == null
              ? "?"
              : result['vpn'] == true
              ? "✓"
              : "✗"}',
        );
        for (final peer in result['peers'] as List? ?? []) {
          lines.add('\n${peer['name']}');
          if (peer['error'] != null) {
            lines.add('• ? Could not verify connection: ${peer['error']}');
            continue;
          }
          final r = peer['report'];
          if (r is! Map) {
            lines.add('• ? Connection report unavailable.');
            continue;
          }
          lines.add(
            '• Path: ${r['initial_path'] ?? "unavailable"} → ${r['selected_path'] ?? "unavailable"} · RTT: ${r['rtt_ms'] ?? "unavailable"} ms',
          );
          lines.add(
            '• Observed for ${r['observation_ms'] ?? "unavailable"} ms · relay: ${r['relay_status'] ?? "unavailable"}',
          );
          lines.add('• ${r['reason'] ?? "No path explanation reported."}');
          final transport = r['transport'] as Map?;
          if (transport != null) {
            lines.add('\nDevice relay connections');
            for (final relay in transport['relays'] as List? ?? []) {
              lines.add(
                '• ${diagnosticObserved(relay['connected'])} ${relay['url']}${relay['preferred'] == true ? " · preferred" : ""}',
              );
            }
            lines.add('Device discovery observations');
            for (final discovery in transport['discovery'] as List? ?? []) {
              lines.add(
                '• ${discovery['url']} · IPv4 ${diagnosticObserved(discovery['ipv4'])} · IPv6 ${diagnosticObserved(discovery['ipv6'])}',
              );
            }
          }
          final local = r['local'] as Map?;
          final mapping = r['router_mapping'] as Map?;
          appendDiagnosticObservation(
            lines,
            'Device observations',
            local ??
                {
                  'network': {'fresh': false, 'report': r['network_report']},
                },
            routerMapping: mapping?['local'] as Map?,
          );
          final remote = r['remote'] as Map?;
          if (remote != null) {
            appendDiagnosticObservation(
              lines,
              'Remote device observations',
              remote,
              routerMapping: mapping?['remote'] as Map?,
            );
          } else {
            lines.add(
              '\n• ? ${r['remote_error'] ?? "Remote device observations unavailable; update the host to collect both ends."}',
            );
          }
          lines.add(
            '• ? Failed candidate validation reasons: unavailable in the transport API. Zero observed paths does not prove no attempts were made.',
          );
        }
        if (result['network_changed'] == true) {
          lines.add(
            '• Network changed during the test; run again on a stable connection.',
          );
        }
      }
      lines.add(
        '\nResults describe this test. Normal device badges show the last request’s path.',
      );
    } catch (e) {
      lines.add(failureMessage(e));
    }
    if (mounted) {
      setState(() {
        running = false;
        report = lines.join('\n');
      });
    }
  }

  @override
  Widget build(BuildContext context) => Scaffold(
    appBar: AppBar(title: const Text('Connection diagnostics')),
    body: SafeArea(
      child: SingleChildScrollView(
        padding: const EdgeInsets.all(24),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Row(
              children: [
                Expanded(
                  child: TextButton.icon(
                    onPressed: running ? null : run,
                    icon: const Icon(Icons.network_check),
                    label: Text(running ? 'Testing…' : 'Run again'),
                  ),
                ),
                const SizedBox(width: 8),
                Expanded(
                  child: TextButton.icon(
                    onPressed: report.isEmpty
                        ? null
                        : () => Clipboard.setData(ClipboardData(text: report)),
                    icon: const Icon(Icons.copy),
                    label: const Text('Copy report'),
                  ),
                ),
              ],
            ),
            if (report.isNotEmpty) ...[
              const SizedBox(height: 8),
              SelectableText(report),
            ],
          ],
        ),
      ),
    ),
  );
}

class ServiceLinks extends StatefulWidget {
  const ServiceLinks({
    super.key,
    required this.backend,
    required this.machine,
    required this.service,
    required this.canResolve,
  });
  final WispnetBackend backend;
  final Machine machine;
  final Map<String, dynamic> service;
  final bool canResolve;
  @override
  State<ServiceLinks> createState() => _ServiceLinksState();
}

class _ServiceLinksState extends State<ServiceLinks> {
  String? localUrl, error;
  bool loading = true;
  @override
  void initState() {
    super.initState();
    load();
  }

  Future<void> load() async {
    setState(() {
      loading = true;
      error = null;
    });
    try {
      final args = {'host': widget.machine.id, 'service': widget.service['id']};
      final settings = widget.service['kind'] == 'tcp'
          ? <String, dynamic>{}
          : await widget.backend.invoke('settings', args);
      if (!mounted) return;
      var url = settings['url'] as String? ?? '';
      if (url.isEmpty) {
        if (!widget.canResolve) {
          throw const FormatException(
            'Go online and refresh this service to get its local URL.',
          );
        }
        final route = await widget.backend.invoke('open', {
          ...args,
          'incarnation': widget.service['incarnation'],
        });
        url = route['url'] as String? ?? '';
      }
      if (url.isEmpty) throw const FormatException('Local URL unavailable.');
      if (mounted) setState(() => localUrl = url);
    } catch (failure) {
      if (mounted) setState(() => error = failureMessage(failure));
    } finally {
      if (mounted) setState(() => loading = false);
    }
  }

  Widget option(
    String label,
    String url,
    String tooltip, {
    String? description,
  }) => Padding(
    padding: const EdgeInsets.only(bottom: 12),
    child: WispPanel(
      child: Row(
        children: [
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  label,
                  style: const TextStyle(fontWeight: FontWeight.bold),
                ),
                if (description != null) ...[
                  const SizedBox(height: 6),
                  Text(
                    description,
                    style: const TextStyle(
                      color: WispColors.muted,
                      fontSize: 13,
                    ),
                  ),
                ],
                const SizedBox(height: 6),
                SelectableText(
                  url,
                  style: const TextStyle(color: WispColors.cyan),
                ),
              ],
            ),
          ),
          WispCopyButton(text: url, tooltip: tooltip, message: 'Link copied'),
        ],
      ),
    ),
  );

  @override
  Widget build(BuildContext context) {
    final publication = widget.service['public_access'] as Map?;
    final publicUrl = publication?['url'] as String?;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        if (localUrl != null)
          option(
            widget.service['kind'] == 'tcp' ? 'Local TCP address' : 'Local URL',
            localUrl!,
            widget.service['kind'] == 'tcp'
                ? 'Copy TCP address'
                : 'Copy local URL',
            description: widget.service['kind'] == 'tcp'
                ? 'Use this address in apps on this device while Wispnet is online. Other devices get their own local address.'
                : 'Open this URL in a browser or app on this device while Wispnet is online. Other devices get their own local URL.',
          )
        else
          WispPanel(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  widget.service['kind'] == 'tcp'
                      ? 'Local TCP address'
                      : 'Local URL',
                  style: const TextStyle(fontWeight: FontWeight.bold),
                ),
                const SizedBox(height: 6),
                Text(
                  loading ? 'Loading…' : error ?? 'Local URL unavailable.',
                  style: TextStyle(
                    color: loading ? WispColors.muted : WispColors.error,
                  ),
                ),
                if (!loading)
                  TextButton(onPressed: load, child: const Text('Retry')),
              ],
            ),
          ),
        if (localUrl == null) const SizedBox(height: 12),
        option(
          'Wispnet URL',
          'wispnet://${widget.machine.id}/${widget.service['id']}',
          'Copy Wispnet link',
          description: 'Use this link to bookmark or share the service across devices with Wispnet installed and access to the host. Wispnet resolves it to a local address on each device.',
        ),
        if (publication?['enabled'] == true &&
            publicUrl != null &&
            publicUrl.isNotEmpty)
          option('Public URL', publicUrl, 'Copy public URL'),
        if (widget.service['kind'] == 'http' && localUrl != null)
          option(
            'WebSocket URL',
            localUrl!.replaceFirst(RegExp(r'^http'), 'ws'),
            'Copy WebSocket URL',
            description: 'Append the service WebSocket path, such as /rpc. Uses the same service permissions.',
          ),
      ],
    );
  }
}

class SettingsForm extends StatefulWidget {
  const SettingsForm({
    super.key,
    required this.backend,
    required this.deviceName,
    required this.onSaved,
    this.onLocalServices,
    this.onOwnerTools,
  });
  final WispnetBackend backend;
  final String deviceName;
  final VoidCallback onSaved;
  final VoidCallback? onLocalServices;
  final VoidCallback? onOwnerTools;
  @override
  State<SettingsForm> createState() => _SettingsFormState();
}

class _SettingsFormState extends State<SettingsForm> {
  late final name = TextEditingController(text: widget.deviceName);
  final port = TextEditingController();
  bool loading = true, saving = false, opening = false;
  String? error;
  String address = '';
  String? section;
  @override
  void initState() {
    super.initState();
    load();
  }

  @override
  void dispose() {
    name.dispose();
    port.dispose();
    super.dispose();
  }

  Future<void> load() async {
    try {
      final result = await widget.backend.invoke('settings');
      if (!mounted) return;
      port.text = '${result['port'] ?? ''}';
      setState(() {
        loading = false;
        address = result['url'] as String? ?? '';
      });
    } catch (e) {
      if (mounted) {
        setState(() {
          loading = false;
          error = failureMessage(e);
        });
      }
    }
  }

  String get browserAddress => address;

  String get mcpAddress => Uri.parse(address).replace(path: '/mcp').toString();

  Widget localAddressRow(
    String url,
    String label,
    String linkKey,
    String copyKey,
  ) => Row(
    children: [
      Expanded(
        child: Align(
          alignment: Alignment.centerLeft,
          child: WispLink(
            key: ValueKey(linkKey),
            text: url,
            tooltip: 'Open $label URL in browser',
            onTap: opening || saving ? null : () => openAddress(url),
          ),
        ),
      ),
      WispCopyButton(
        key: ValueKey(copyKey),
        text: url,
        tooltip: 'Copy $label URL',
        message: '$label URL copied',
      ),
    ],
  );

  Future<void> openAddress(String url) async {
    if (opening || url.isEmpty) return;
    setState(() {
      opening = true;
      error = null;
    });
    try {
      await widget.backend.invoke('launch', {'url': url});
    } catch (e) {
      if (mounted) setState(() => error = failureMessage(e));
    } finally {
      if (mounted) setState(() => opening = false);
    }
  }

  Future<void> save() async {
    final raw = port.text.trim();
    final number = int.tryParse(raw);
    if (number == null || (number < 1024 || number > 65535)) {
      setState(() => error = 'Port must be between 1024 and 65535.');
      return;
    }
    final value = name.text.trim();
    if (!RegExp(r'^[a-z0-9][a-z0-9._-]{0,63}$').hasMatch(value)) {
      setState(
        () => error = 'Use 1–64 lowercase letters, numbers, dots, underscores or hyphens; start with a letter or number.',
      );
      return;
    }
    setState(() {
      saving = true;
      error = null;
    });
    try {
      await widget.backend.invoke('saveSettings', {
        'port': number,
        'name': value,
      });
      if (mounted) widget.onSaved();
    } catch (e) {
      if (mounted) setState(() => error = failureMessage(e));
    } finally {
      if (mounted) setState(() => saving = false);
    }
  }

  void showSection(String? value) {
    FocusManager.instance.primaryFocus?.unfocus();
    Scrollable.maybeOf(context)?.position.jumpTo(0);
    setState(() => section = value);
  }

  Widget sectionRow(
    IconData icon,
    String title,
    String description,
    VoidCallback onTap,
  ) => Material(
    type: MaterialType.transparency,
    child: ListTile(
      contentPadding: EdgeInsets.zero,
      leading: Icon(icon),
      title: Text(title),
      subtitle: Text(description),
      trailing: const Icon(Icons.chevron_right),
      onTap: onTap,
    ),
  );

  @override
  Widget build(BuildContext context) => Column(
    crossAxisAlignment: CrossAxisAlignment.stretch,
    children: [
      if (section == null) ...[
        sectionRow(
          Icons.devices_outlined,
          'Device',
          'Name and network port',
          () => showSection('Device'),
        ),
        sectionRow(
          Icons.dns_outlined,
          'Services',
          'Local services and registry',
          () => showSection('Services'),
        ),
        if (widget.onOwnerTools != null)
          sectionRow(
            Icons.lock_outline,
            'Access & pairing',
            'Device access, paired devices and requests',
            widget.onOwnerTools!,
          ),
        sectionRow(
          Icons.network_check,
          'Connections',
          'MCP setup and diagnostics',
          () => showSection('Connections'),
        ),
      ] else ...[
        Row(
          children: [
            IconButton(
              tooltip: 'Back to settings',
              icon: const Icon(Icons.arrow_back),
              onPressed: saving ? null : () => showSection(null),
            ),
            Expanded(
              child: Text(
                section!,
                style: Theme.of(context).textTheme.titleLarge,
              ),
            ),
          ],
        ),
        const SizedBox(height: 16),
      ],
      if (error != null)
        Padding(
          padding: const EdgeInsets.symmetric(vertical: 12),
          child: Text(error!, style: const TextStyle(color: WispColors.error)),
        ),
      if (section == 'Device') ...[
        TextField(
          controller: name,
          enabled: !saving,
          decoration: const InputDecoration(labelText: 'Device name'),
        ),
        const SizedBox(height: 12),
        Material(
          type: MaterialType.transparency,
          child: ExpansionTile(
            tilePadding: EdgeInsets.zero,
            title: const Text('Advanced'),
            children: [
              const Text(
                'One port for all Wispnet services. Default: 7842. Changing it reconnects active sessions.',
                style: TextStyle(color: WispColors.muted),
              ),
              const SizedBox(height: 16),
              TextField(
                controller: port,
                enabled: !loading && !saving,
                keyboardType: TextInputType.number,
                decoration: const InputDecoration(labelText: 'Wispnet port'),
              ),
              const SizedBox(height: 16),
            ],
          ),
        ),
        const SizedBox(height: 12),
        FilledButton(
          onPressed: loading || saving ? null : save,
          child: Text(
            loading
                ? 'Loading…'
                : saving
                ? 'Saving…'
                : 'Save',
          ),
        ),
      ],
      if (section == 'Services') ...[
        if (widget.onLocalServices != null) ...[
          const SizedBox(height: 12),
          OutlinedButton.icon(
            onPressed: saving ? null : widget.onLocalServices,
            icon: const Icon(Icons.dns_outlined),
            label: const Text('Local services'),
          ),
        ],
      ],
      if (section == 'Connections') ...[
        const SizedBox(height: 12),
        OutlinedButton.icon(
          onPressed: saving
              ? null
              : () {
                  FocusManager.instance.primaryFocus?.unfocus();
                  Navigator.of(context).push(
                    MaterialPageRoute<void>(
                      builder: (_) =>
                          ConnectionDiagnostics(backend: widget.backend),
                    ),
                  );
                },
          icon: const Icon(Icons.network_check),
          label: const Text('Connection diagnostics'),
        ),
      ],
      if (section == 'Services' && address.isNotEmpty) ...[
        const SizedBox(height: 16),
        const Text('Service registry'),
        const SizedBox(height: 4),
        const Text(
          'Services available to this device and their local URLs.',
          style: TextStyle(color: WispColors.muted),
        ),
        const SizedBox(height: 8),
        localAddressRow(
          browserAddress,
          'registry',
          'settings-address',
          'settings-registry-copy',
        ),
        const SizedBox(height: 16),
      ],
      if (section == 'Connections' && address.isNotEmpty) ...[
        const Text('MCP (Model Context Protocol)'),
        const SizedBox(height: 4),
        const Text(
          'Add this URL to your AI assistant’s MCP client. No token or sign-in needed. Works on this device while local access is online.',
          style: TextStyle(color: WispColors.muted),
        ),
        const SizedBox(height: 8),
        localAddressRow(
          mcpAddress,
          'MCP',
          'settings-mcp-address',
          'settings-mcp-copy',
        ),
      ],
    ],
  );
}
