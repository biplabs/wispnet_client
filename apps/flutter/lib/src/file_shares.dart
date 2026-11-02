import 'dart:async';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import 'backend.dart';
import 'file_transfer.dart';
import '../ui.dart';

String _shareFailure(Object error) => error is FormatException
    ? error.message
    : error is PlatformException
    ? error.message ?? error.code
    : error.toString();

class SharedFileDownload {
  SharedFileDownload({required this.request, required this.name});
  final Map<String, dynamic> request;
  final String name;
  Map<String, dynamic> status = {'state': 'starting'};
  int generation = 0;
  String? failure;
  bool get finished =>
      const ['complete', 'failed', 'cancelled'].contains(status['state']);
  String? get transfer => status['transfer'] as String?;
}

/// Tracks native downloads while the browser sheet is closed. File contents
/// remain in the native transfer service; Flutter only holds job metadata.
class SharedFileDownloads extends ChangeNotifier {
  SharedFileDownloads(this.backend);
  final WispnetBackend backend;
  final List<SharedFileDownload> jobs = [];
  final Set<String> _dismissed = {};
  Timer? _timer;
  bool _polling = false, _disposed = false;

  void _changed() {
    if (!_disposed) notifyListeners();
  }

  Future<void> start(SharedFileDownload job, {bool resume = true}) async {
    if (jobs.contains(job) && !job.finished) return;
    if (!jobs.contains(job)) jobs.add(job);
    final superseded = job.transfer;
    if (superseded != null) _dismissed.add(superseded);
    final generation = ++job.generation;
    job.status = {'state': 'starting'};
    job.failure = null;
    _changed();
    try {
      final status = await backend.invoke('shareGet', {
        ...job.request,
        'resume': resume,
      });
      if (generation != job.generation) return;
      job.status = status;
      if (job.transfer == null && !job.finished) {
        throw const FormatException('The download could not be started.');
      }
    } catch (error) {
      job.status = {'state': 'failed'};
      job.failure = _shareFailure(error);
    }
    _changed();
    if (!_disposed && jobs.any((job) => !job.finished)) {
      _timer ??= Timer.periodic(const Duration(seconds: 1), (_) => poll());
    }
  }

  void restore(List<dynamic> transfers) {
    for (final entry in transfers.whereType<Map>()) {
      final transfer = entry['transfer'];
      final request = entry['request'];
      if (transfer is! String || request is! Map) continue;
      if (_dismissed.contains(transfer)) continue;
      if (jobs.any((job) => job.transfer == transfer)) continue;
      final job = SharedFileDownload(
        request: request.cast<String, dynamic>(),
        name:
            entry['name'] as String? ??
            request['name'] as String? ??
            'Download',
      )..status = entry.cast<String, dynamic>();
      jobs.add(job);
    }
    if (!_disposed && jobs.any((job) => !job.finished)) {
      _timer ??= Timer.periodic(const Duration(seconds: 1), (_) => poll());
    }
  }

  void clearFinished() {
    _dismissed.addAll(
      jobs
          .where((job) => job.finished)
          .map((job) => job.transfer)
          .whereType<String>(),
    );
    jobs.removeWhere((job) => job.finished);
    _changed();
  }

  Future<void> poll() async {
    if (_polling || _disposed) return;
    _polling = true;
    try {
      for (final job in List.of(jobs)) {
        if (job.finished || job.transfer == null) continue;
        final generation = job.generation;
        try {
          final status = await backend.invoke('fileStatus', {
            'transfer': job.transfer,
          });
          if (generation != job.generation) continue;
          job.status = status;
          job.failure = null;
        } catch (error) {
          if (generation != job.generation) continue;
          if (_missingHandle(job, error)) continue;
          // Preserve the handle: an interrupted owner connection does not mean
          // the native job stopped, and must never start a second download.
          job.failure = 'Waiting for the local device: ${_shareFailure(error)}';
        }
      }
      _changed();
    } finally {
      _polling = false;
      if (!jobs.any((job) => !job.finished)) {
        _timer?.cancel();
        _timer = null;
      }
    }
  }

  Future<void> cancel(SharedFileDownload job) async {
    if (job.transfer == null || job.finished) return;
    final generation = ++job.generation;
    try {
      final status = await backend.invoke('fileCancel', {
        'transfer': job.transfer,
      });
      if (generation != job.generation) return;
      job.status = status;
      job.failure = null;
    } catch (error) {
      if (generation != job.generation) return;
      if (_missingHandle(job, error)) {
        _changed();
        return;
      }
      job.failure = _shareFailure(error);
    }
    _changed();
  }

  bool _missingHandle(SharedFileDownload job, Object error) {
    if (!_shareFailure(error).toLowerCase().contains('handle unavailable')) {
      return false;
    }
    job.status = {
      ...job.status,
      'state': 'failed',
      'error': {
        'code': 'TRANSFER_UNAVAILABLE',
        'message': 'This download is no longer active. Resume to recover its saved data.',
      },
    };
    job.failure = null;
    return true;
  }

  @override
  void dispose() {
    _disposed = true;
    _timer?.cancel();
    super.dispose();
  }
}

class SharedFilesBrowser extends StatefulWidget {
  const SharedFilesBrowser({
    super.key,
    required this.backend,
    required this.machine,
    required this.service,
    required this.downloads,
    required this.desktop,
  });
  final WispnetBackend backend;
  final Machine machine;
  final Map<String, dynamic> service;
  final SharedFileDownloads downloads;
  final bool desktop;
  @override
  State<SharedFilesBrowser> createState() => _SharedFilesBrowserState();
}

class _SharedFilesBrowserState extends State<SharedFilesBrowser> {
  String path = '';
  String? cursor, failure;
  List<Map<String, dynamic>> entries = [];
  bool loading = false, choosing = false;
  int generation = 0;
  Map<String, dynamic> get target => {
    'host': widget.machine.id,
    'service': widget.service['id'],
    'incarnation': widget.service['incarnation'],
  };

  @override
  void initState() {
    super.initState();
    unawaited(load(''));
  }

  Future<void> load(String nextPath, {bool more = false}) async {
    final requestGeneration = ++generation;
    setState(() {
      loading = true;
      failure = null;
    });
    try {
      final result = await widget.backend.invoke('shareList', {
        ...target,
        'path': nextPath,
        if (more && cursor != null) 'cursor': cursor,
      });
      if (!mounted || requestGeneration != generation) return;
      final next = (result['entries'] as List? ?? [])
          .cast<Map>()
          .map((entry) => entry.cast<String, dynamic>())
          .toList();
      setState(() {
        path = nextPath;
        entries = more ? [...entries, ...next] : next;
        cursor = result['next_cursor'] as String?;
      });
    } catch (error) {
      if (mounted && requestGeneration == generation) {
        setState(() => failure = _shareFailure(error));
      }
    } finally {
      if (mounted && requestGeneration == generation) {
        setState(() => loading = false);
      }
    }
  }

  Future<void> download(Map<String, dynamic> entry) async {
    if (choosing) return;
    setState(() {
      choosing = true;
      failure = null;
    });
    try {
      final name = entry['name'] as String;
      if (name.isEmpty ||
          name == '.' ||
          name == '..' ||
          name.contains('/') ||
          name.contains('\\') ||
          name.contains('\u0000')) {
        throw const FormatException('The shared item has an invalid name.');
      }
      final destination = await widget.backend.invoke(
        widget.desktop ? 'pickFolder' : 'pickDownloadFolder',
        {'title': 'Save $name in a folder'},
      );
      final folder = destination['path'] as String?;
      if (folder == null || folder.isEmpty || !mounted) return;
      final job = SharedFileDownload(
        name: name,
        request: {
          ...target,
          'path': entry['path'],
          'local_path': widget.desktop
              ? '$folder${Platform.pathSeparator}$name'
              : folder,
          'name': name,
          if (entry['is_dir'] == true) 'recursive': true,
          'resume': true,
        },
      );
      await widget.downloads.start(job);
    } catch (error) {
      if (mounted) setState(() => failure = _shareFailure(error));
    } finally {
      if (mounted) setState(() => choosing = false);
    }
  }

  Widget downloadRow(SharedFileDownload job) {
    return SharedDownloadTile(downloads: widget.downloads, job: job);
  }

  @override
  Widget build(BuildContext context) => AnimatedBuilder(
    animation: widget.downloads,
    builder: (context, _) => Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        const Text(
          'Read-only share. Choose files or folders to download to this device. Inbox access is separate.',
        ),
        const SizedBox(height: 12),
        Row(
          children: [
            if (path.isNotEmpty)
              IconButton(
                tooltip: 'Parent folder',
                onPressed: loading
                    ? null
                    : () => load(
                        path
                            .split('/')
                            .take(path.split('/').length - 1)
                            .join('/'),
                      ),
                icon: const Icon(Icons.arrow_upward),
              ),
            Expanded(
              child: Text(
                path.isEmpty
                    ? '${widget.service['id']} /'
                    : '${widget.service['id']} / $path',
              ),
            ),
            IconButton(
              tooltip: 'Download this folder',
              onPressed: loading || choosing || failure != null
                  ? null
                  : () => download({
                      'name': path.isEmpty
                          ? widget.service['id']
                          : path.split('/').last,
                      'path': path,
                      'is_dir': true,
                    }),
              icon: const Icon(Icons.download),
            ),
            IconButton(
              tooltip: 'Refresh shared files',
              onPressed: loading ? null : () => load(path),
              icon: const Icon(Icons.refresh),
            ),
          ],
        ),
        if (loading) const LinearProgressIndicator(),
        if (failure != null)
          Text(failure!, style: const TextStyle(color: WispColors.error)),
        if (!loading && entries.isEmpty && failure == null)
          const Padding(
            padding: EdgeInsets.all(16),
            child: Text('This folder is empty.'),
          ),
        for (final entry in entries)
          WispListItem(
            key: ValueKey('shared-entry-${entry['path']}'),
            title: Text(entry['name'] as String),
            leading: Icon(
              entry['is_dir'] == true
                  ? Icons.folder_outlined
                  : Icons.description_outlined,
              color: WispColors.cyan,
            ),
            subtitle: entry['is_dir'] == true
                ? const Text('Folder')
                : Text(fileSize((entry['size_bytes'] as num?) ?? 0)),
            onTap: entry['is_dir'] == true && !loading
                ? () => load(entry['path'] as String)
                : null,
            actions: [
              IconButton(
                tooltip: entry['is_dir'] == true
                    ? 'Download folder ${entry['name']}'
                    : 'Download ${entry['name']}',
                onPressed: choosing ? null : () => download(entry),
                icon: const Icon(Icons.download),
              ),
            ],
          ),
        if (cursor != null)
          TextButton(
            onPressed: loading ? null : () => load(path, more: true),
            child: const Text('Load more files'),
          ),
        if (widget.downloads.jobs.any(
          (job) =>
              job.request['host'] == widget.machine.id &&
              job.request['service'] == widget.service['id'],
        )) ...[
          const SizedBox(height: 16),
          const Text(
            'Downloads',
            style: TextStyle(fontWeight: FontWeight.bold),
          ),
          const Text('Downloads continue when this browser closes.'),
          for (final job in widget.downloads.jobs.where(
            (job) =>
                job.request['host'] == widget.machine.id &&
                job.request['service'] == widget.service['id'],
          ))
            downloadRow(job),
        ],
      ],
    ),
  );
}

class SharedDownloadsList extends StatelessWidget {
  const SharedDownloadsList({super.key, required this.downloads});
  final SharedFileDownloads downloads;
  @override
  Widget build(BuildContext context) => AnimatedBuilder(
    animation: downloads,
    builder: (context, _) => Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        const Text('Downloads continue when this panel closes.'),
        if (downloads.jobs.isEmpty) const Text('No downloads.'),
        for (final job in downloads.jobs)
          SharedDownloadTile(downloads: downloads, job: job),
      ],
    ),
  );
}

class SharedDownloadTile extends StatelessWidget {
  const SharedDownloadTile({
    super.key,
    required this.downloads,
    required this.job,
  });
  final SharedFileDownloads downloads;
  final SharedFileDownload job;
  @override
  Widget build(BuildContext context) {
    final state = job.status['state'] as String? ?? 'starting';
    final size = (job.status['size_bytes'] as num?)?.toInt() ?? 0;
    final verified = (job.status['verified_bytes'] as num?)?.toInt() ?? 0;
    final filesTotal = (job.status['files_total'] as num?)?.toInt();
    final filesComplete = (job.status['files_complete'] as num?)?.toInt() ?? 0;
    final folder = job.request['recursive'] == true;
    final error = job.status['error'];
    final changed = error is Map && error['code'] == 'STALE_VERSION';
    final problem =
        job.failure ?? (error is Map ? error['message'] as String? : null);
    return Padding(
      padding: const EdgeInsets.only(top: 8),
      child: WispPanel(
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.stretch,
          children: [
            Text(job.name),
            Text(
              state == 'complete'
                  ? 'Downloaded · ${fileSize(size)}'
                  : '${job.status['phase'] ?? state} · ${fileSize(verified)} / ${fileSize(size)}',
            ),
            if (folder && filesTotal != null)
              Text('$filesComplete / $filesTotal files'),
            if (!job.finished)
              LinearProgressIndicator(
                value: size > 0 ? (verified / size).clamp(0, 1) : null,
              ),
            if (problem != null)
              Text(problem, style: const TextStyle(color: WispColors.error)),
            if (!job.finished && job.transfer != null)
              TextButton(
                onPressed: () => downloads.cancel(job),
                child: const Text('Cancel download'),
              ),
            if (state == 'failed' || state == 'cancelled')
              TextButton(
                onPressed: () => downloads.start(job, resume: !changed),
                child: Text(changed ? 'Restart download' : 'Resume download'),
              ),
          ],
        ),
      ),
    );
  }
}

class FileShareEditor extends StatefulWidget {
  const FileShareEditor({
    super.key,
    required this.backend,
    required this.clients,
    required this.onSave,
    this.id = '',
    this.root = '',
    this.allow = const [],
    this.android = false,
    this.folderLabel = '',
    this.folderAvailable = true,
    this.onRemove,
    this.folderService = false,
    this.chooseMode = false,
    this.mode = 'share',
    this.onSaveFolder,
  });
  final WispnetBackend backend;
  final List<Map<String, dynamic>> clients;
  final Future<void> Function(String id, String root, List<String> allow)
  onSave;
  final String id, root;
  final List<String> allow;
  final bool android, folderAvailable;
  final String folderLabel;
  final Future<bool> Function()? onRemove;
  final bool folderService, chooseMode;
  final String mode;
  final Future<void> Function(
    String mode,
    String id,
    String root,
    List<String> allow,
  )?
  onSaveFolder;
  @override
  State<FileShareEditor> createState() => _FileShareEditorState();
}

class _FileShareEditorState extends State<FileShareEditor> {
  late final id = TextEditingController(text: widget.id);
  late final root = TextEditingController(text: widget.root);
  late final allowed = widget.allow.toSet();
  late String folderLabel = widget.folderLabel;
  late bool folderAvailable = widget.folderAvailable;
  late String mode = widget.mode;
  bool get inbox => mode == 'inbox';
  bool busy = false;
  String? failure;
  String? defaultName;

  @override
  void dispose() {
    id.dispose();
    root.dispose();
    super.dispose();
  }

  Future<void> pick() async {
    setState(() {
      busy = true;
      failure = null;
    });
    try {
      final result = await widget.backend.invoke(
        widget.android
            ? inbox
                  ? 'pickReceivingFolder'
                  : 'pickShareFolder'
            : 'pickFolder',
        {
          'title': inbox
              ? 'Choose a receiving folder'
              : 'Choose a shared folder',
        },
      );
      final selection = result[widget.android ? 'folder' : 'path'];
      if (mounted && selection is String && selection.isNotEmpty) {
        setState(() {
          root.text = selection;
          folderLabel =
              result['label'] as String? ??
              selection
                  .split(RegExp(r'[/\\]'))
                  .where((part) => part.isNotEmpty)
                  .lastOrNull ??
              '';
          if (widget.id.isEmpty &&
              (id.text.trim().isEmpty || id.text == defaultName)) {
            var suggested = folderLabel
                .toLowerCase()
                .replaceAll(RegExp(r'[^a-z0-9._-]+'), '-')
                .replaceAll(RegExp(r'^[^a-z0-9]+'), '');
            if (suggested.isEmpty) {
              suggested = inbox ? 'incoming' : 'shared-files';
            }
            if (suggested.length > 64) suggested = suggested.substring(0, 64);
            if (const ['files', 'shell'].contains(suggested)) {
              suggested = '$suggested-folder';
            }
            defaultName = suggested;
            id.text = suggested;
          }
          folderAvailable = true;
        });
      }
    } catch (error) {
      if (mounted) setState(() => failure = _shareFailure(error));
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  Future<void> save() async {
    setState(() {
      busy = true;
      failure = null;
    });
    try {
      final name = id.text.trim();
      if (!RegExp(r'^[a-z0-9][a-z0-9._-]{0,63}$').hasMatch(name) ||
          const ['files', 'shell'].contains(name) &&
              !(name == 'files' && widget.id == 'files' && inbox)) {
        throw const FormatException(
          'Enter a service ID using lowercase letters, numbers, dots, hyphens or underscores. “files” and “shell” are reserved.',
        );
      }
      if (widget.android
          ? root.text.isEmpty || !folderAvailable
          : !root.text.trim().startsWith('/')) {
        throw const FormatException('Choose an existing folder before saving.');
      }
      if (widget.onSaveFolder != null) {
        await widget.onSaveFolder!(
          mode,
          name,
          root.text.trim(),
          allowed.toList(),
        );
      } else {
        await widget.onSave(name, root.text.trim(), allowed.toList());
      }
      if (mounted && wispPageIsCurrent(context)) Navigator.pop(context);
    } catch (error) {
      if (mounted) setState(() => failure = _shareFailure(error));
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  Future<void> remove() async {
    setState(() {
      busy = true;
      failure = null;
    });
    try {
      if (await widget.onRemove!() && mounted && wispPageIsCurrent(context)) {
        Navigator.pop(context);
      }
    } catch (error) {
      if (mounted) setState(() => failure = _shareFailure(error));
    } finally {
      if (mounted) setState(() => busy = false);
    }
  }

  @override
  Widget build(BuildContext context) => Column(
    crossAxisAlignment: CrossAxisAlignment.stretch,
    children: [
      if (widget.chooseMode)
        DropdownButtonFormField<String>(
          key: const ValueKey('folder-mode'),
          initialValue: mode,
          decoration: const InputDecoration(labelText: 'Use this folder to'),
          items: const [
            DropdownMenuItem(value: 'inbox', child: Text('Receive files')),
            DropdownMenuItem(value: 'share', child: Text('Share files')),
          ],
          onChanged: busy
              ? null
              : (value) => setState(() {
                  mode = value!;
                  allowed.clear();
                  if (widget.android) {
                    root.clear();
                    folderLabel = '';
                    folderAvailable = false;
                  }
                }),
        ),
      if (widget.chooseMode) const SizedBox(height: 12),
      Text(
        inbox
            ? 'Allow selected devices to send files anytime.'
            : 'Allow selected devices to read files anytime. Read only.',
      ),
      const SizedBox(height: 12),
      if (root.text.isNotEmpty)
        WispListItem(
          key: const ValueKey('share-selected-folder'),
          contentPadding: EdgeInsets.zero,
          leading: const Icon(Icons.folder_outlined, color: WispColors.cyan),
          title: Text(widget.android ? folderLabel : root.text),
          subtitle: !folderAvailable
              ? const Text(
                  'Folder unavailable. Choose it again.',
                  style: TextStyle(color: WispColors.error),
                )
              : null,
        ),
      const SizedBox(height: 8),
      OutlinedButton.icon(
        onPressed: busy ? null : pick,
        icon: const Icon(Icons.folder_open),
        label: Text(
          widget.folderService ? 'Choose folder' : 'Choose shared folder',
        ),
      ),
      const SizedBox(height: 12),
      WispServiceIdField(
        fieldKey: const ValueKey('share-name'),
        controller: id,
        enabled: !busy,
        hintText: inbox ? 'incoming-datasets' : 'checkpoints',
      ),
      const SizedBox(height: 12),
      WispPermissionCard(
        children: [
          for (final client in widget.clients)
            WispToggleListTile(
              key: ValueKey('share-allow-${client['peer']}'),
              contentPadding: EdgeInsets.zero,
              title: Text(client['name'] as String),
              subtitle: Text(client['peer'] as String),
              value: allowed.contains(client['peer']),
              onChanged:
                  busy ||
                      widget.android && (root.text.isEmpty || !folderAvailable)
                  ? null
                  : (value) => setState(() {
                      if (value!) {
                        allowed.add(client['peer'] as String);
                      } else {
                        allowed.remove(client['peer']);
                      }
                    }),
            ),
        ],
      ),
      const SizedBox(height: 12),
      if (failure != null)
        Text(failure!, style: const TextStyle(color: WispColors.error)),
      const SizedBox(height: 12),
      FilledButton(
        onPressed:
            busy || widget.android && (root.text.isEmpty || !folderAvailable)
            ? null
            : save,
        child: const Text('Save'),
      ),
      if (widget.onRemove != null) ...[
        const SizedBox(height: 12),
        OutlinedButton(
          onPressed: busy ? null : remove,
          style: wispDangerButtonStyle(),
          child: Text(widget.folderService ? 'Remove folder' : 'Remove share'),
        ),
      ],
    ],
  );
}
