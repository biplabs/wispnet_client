import 'dart:async';
import 'dart:io';

import 'package:desktop_drop/desktop_drop.dart';
import 'package:flutter/material.dart';

import 'backend.dart';
import 'sheets.dart';
import '../ui.dart';

String _fileName(String path) => path.split(Platform.pathSeparator).last;

String fileSize(num bytes) {
  if (bytes < 1024) return '$bytes B';
  if (bytes < 1024 * 1024) return '${(bytes / 1024).toStringAsFixed(1)} KB';
  if (bytes < 1024 * 1024 * 1024) {
    return '${(bytes / (1024 * 1024)).toStringAsFixed(1)} MB';
  }
  return '${(bytes / (1024 * 1024 * 1024)).toStringAsFixed(2)} GB';
}

class DesktopFileTransfer {
  DesktopFileTransfer({
    required this.machine,
    required this.incarnation,
    required this.workspace,
    required this.path,
    this.service = 'files',
    this.displayName,
    this.sourceSizeBytes = 0,
    this.resume = true,
    this.elapsed,
  }) {
    sizeBytes = sourceSizeBytes;
  }
  final Machine machine;
  final String incarnation, workspace, path, service;
  final bool resume;
  final String? displayName;
  final int sourceSizeBytes;
  String get name => displayName ?? _fileName(path);
  String get hostName => machine.name;
  String get remotePath => name;
  String state = 'queued', phase = 'preparing';
  String? transfer, error, errorCode;
  int verifiedBytes = 0, sizeBytes = 0, copiedBytes = 0;
  bool cancelling = false;
  bool _cancelSent = false;
  final _clock = Stopwatch()..start();
  final Duration Function()? elapsed;
  Duration? _sampleAt;
  int? _sampleBytes;
  double? _bytesPerSecond;
  final _completion = Completer<void>();
  Future<void> get done => _completion.future;
  bool get finished =>
      const ['complete', 'failed', 'cancelled'].contains(state);
  bool get uploading =>
      state == 'running' &&
      const ['sending', 'transferring', 'uploading'].contains(phase) &&
      !cancelling &&
      error == null;
  Duration get _now => elapsed?.call() ?? _clock.elapsed;
  double? get bytesPerSecond {
    if (!uploading) return null;
    // A delayed native reply must not keep advertising an old moving rate.
    if (_sampleAt != null && _now - _sampleAt! >= const Duration(seconds: 3)) {
      return 0;
    }
    return _bytesPerSecond;
  }

  void _resetSpeed() {
    _sampleAt = null;
    _sampleBytes = null;
    _bytesPerSecond = null;
  }

  void _measureSpeed() {
    if (!uploading) {
      _resetSpeed();
      return;
    }
    final now = _now;
    final previousAt = _sampleAt;
    final previousBytes = _sampleBytes;
    if (previousAt == null ||
        previousBytes == null ||
        now < previousAt ||
        verifiedBytes < previousBytes) {
      // The first observation may include a restored/resumed prefix. It is a
      // baseline, never bytes uploaded during this process's sampling window.
      _bytesPerSecond = null;
    } else if (now == previousAt) {
      return;
    } else {
      _bytesPerSecond =
          (verifiedBytes - previousBytes) *
          Duration.microsecondsPerSecond /
          (now - previousAt).inMicroseconds;
    }
    _sampleAt = now;
    _sampleBytes = verifiedBytes;
  }

  void update(Map<String, dynamic> status) {
    transfer = status['transfer'] as String? ?? transfer;
    state = status['state'] as String? ?? state;
    phase = status['phase'] as String? ?? phase;
    verifiedBytes =
        (status['verified_bytes'] as num?)?.toInt() ?? verifiedBytes;
    sizeBytes = (status['size_bytes'] as num?)?.toInt() ?? sizeBytes;
    copiedBytes = (status['copied_bytes'] as num?)?.toInt() ?? copiedBytes;
    final failure = status['error'];
    errorCode = failure is Map ? failure['code'] as String? : null;
    error = failure is Map
        ? failure['message'] as String? ?? 'File transfer failed.'
        : null;
    if (finished) cancelling = false;
    _measureSpeed();
    finish();
  }

  void finish() {
    if (finished) _resetSpeed();
    if (finished && !_completion.isCompleted) _completion.complete();
  }
}

/// Native jobs keep running when a sheet or the tray window is hidden. Only
/// paths and progress cross this adapter; the daemon streams the file bytes.
class DesktopFileTransfers extends ChangeNotifier {
  DesktopFileTransfers(this.backend, {this.elapsed});
  final WispnetBackend backend;
  final Duration Function()? elapsed;
  final List<DesktopFileTransfer> jobs = [];
  // Android owns a bounded native queue in its foreground service. Submit all
  // selected jobs there so closing/recreating the Activity does not lose them.
  bool nativeQueue = false;
  bool _working = false, _disposed = false;
  bool _restored = false, _restoring = false;
  Timer? _timer;
  Timer? _speedTimer;
  Completer<void>? _wake;

  void _trackSpeed() {
    if (_disposed || !jobs.any((job) => !job.finished)) {
      _speedTimer?.cancel();
      _speedTimer = null;
      return;
    }
    if (_speedTimer != null) return;
    _speedTimer = Timer.periodic(const Duration(seconds: 1), (_) {
      if (_disposed || !jobs.any((job) => !job.finished)) {
        _speedTimer?.cancel();
        _speedTimer = null;
        return;
      }
      // Rebuild rates when samples expire, even while a native call is waiting.
      notifyListeners();
    });
  }

  Future<void> _pause() {
    final wake = Completer<void>();
    _wake = wake;
    _timer = Timer(const Duration(seconds: 1), () {
      if (!wake.isCompleted) wake.complete();
    });
    return wake.future;
  }

  void _wakeUp() {
    _timer?.cancel();
    final wake = _wake;
    if (wake != null && !wake.isCompleted) wake.complete();
  }

  List<DesktopFileTransfer> enqueue({
    required Machine machine,
    required String incarnation,
    String service = 'files',
    String workspace = '',
    required List<String> paths,
    Map<String, String> names = const {},
    Map<String, int> sizes = const {},
    bool resume = true,
  }) {
    final added = paths
        .map(
          (path) => DesktopFileTransfer(
            machine: machine,
            incarnation: incarnation,
            service: service,
            workspace: workspace,
            path: path,
            displayName: names[path],
            sourceSizeBytes: sizes[path] ?? 0,
            resume: resume,
            elapsed: elapsed,
          ),
        )
        .toList();
    jobs.addAll(added);
    _trackSpeed();
    notifyListeners();
    _wakeUp();
    unawaited(_run());
    return added;
  }

  Future<void> restore(Map<String, Machine> machines) async {
    if (_disposed || _restored || _restoring) return;
    _restoring = true;
    try {
      final response = await backend.invoke('fileList');
      if (_disposed) return;
      for (final entry in response['transfers'] as List? ?? const []) {
        if (entry is! Map) continue;
        final status = entry.cast<String, dynamic>();
        final id = status['transfer'] as String?;
        final host = status['host'] as String?;
        final incarnation = status['incarnation'] as String?;
        final workspace = status['workspace'] as String?;
        final path = status['local_path'] as String?;
        final name =
            status['name'] as String? ?? status['remote_path'] as String?;
        if (id == null ||
            host == null ||
            incarnation == null ||
            workspace == null ||
            path == null ||
            name == null ||
            jobs.any((job) => job.transfer == id)) {
          continue;
        }
        final job = DesktopFileTransfer(
          machine:
              machines[host] ??
              Machine(host, {'name': status['host_name'] ?? 'Device'}),
          incarnation: incarnation,
          service: status['service'] as String? ?? 'files',
          workspace: workspace,
          path: path,
          displayName: name,
          sourceSizeBytes: (status['size_bytes'] as num?)?.toInt() ?? 0,
          elapsed: elapsed,
        )..update(status);
        jobs.add(job);
      }
      _restored = true;
      _trackSpeed();
      notifyListeners();
      unawaited(_run());
    } catch (_) {
      // Networking may still be starting. The next online snapshot retries.
    } finally {
      _restoring = false;
    }
  }

  Future<void> _run() async {
    if (_working || _disposed) return;
    _working = true;
    try {
      while (!_disposed) {
        var active = jobs
            .where((job) => !job.finished && job.transfer != null)
            .toList();
        while (!_disposed && (nativeQueue || active.isEmpty)) {
          final job = jobs
              .where((job) => job.state == 'queued' && job.transfer == null)
              .firstOrNull;
          if (job == null) break;
          job.state = 'starting';
          notifyListeners();
          try {
            job.update(
              await backend.invoke('fileSend', {
                'host': job.machine.id,
                if (job.service != 'files') 'service': job.service,
                'incarnation': job.incarnation,
                'workspace': job.workspace,
                'local_path': job.path,
                'remote_path': job.remotePath,
                'resume': job.resume,
              }),
            );
            if (job.transfer == null && !job.finished) {
              throw const FormatException(
                'The file transfer could not be started.',
              );
            }
          } catch (error) {
            job.state = 'failed';
            job.error = failureMessage(error);
            job.finish();
          }
          active = jobs
              .where((job) => !job.finished && job.transfer != null)
              .toList();
        }
        for (final job in active) {
          if (_disposed) break;
          try {
            if (job.cancelling && !job._cancelSent) {
              try {
                job.update(
                  await backend.invoke('fileCancel', {
                    'transfer': job.transfer,
                  }),
                );
                job._cancelSent = true;
              } catch (error) {
                final message = failureMessage(error);
                if (message.contains('handle unavailable')) rethrow;
                job.error = 'Waiting to cancel: $message';
              }
            }
            if (!job.finished) {
              job.update(
                await backend.invoke('fileStatus', {'transfer': job.transfer}),
              );
            }
          } catch (error) {
            final message = failureMessage(error);
            if (message.contains('handle unavailable')) {
              job.state = 'failed';
              job.error = message;
              job.finish();
            } else {
              // A temporary owner-socket failure must not start another job
              // while this native transfer may still be running.
              job.error = 'Waiting for the local device: $message';
            }
          }
        }
        if (!_disposed) {
          _trackSpeed();
          notifyListeners();
        }
        if (_disposed) break;
        final pending = jobs.any(
          (job) => job.state == 'queued' && job.transfer == null,
        );
        final running = jobs.any(
          (job) => !job.finished && job.transfer != null,
        );
        if (pending && (nativeQueue || !running)) continue;
        if (!running) break;
        await _pause();
      }
    } finally {
      _working = false;
    }
  }

  void cancel(DesktopFileTransfer job) {
    if (job.finished || job.cancelling) return;
    job._resetSpeed();
    if (job.state == 'queued' && job.transfer == null) {
      job.state = 'cancelled';
      job.finish();
    } else {
      job.cancelling = true;
      _wakeUp();
    }
    _trackSpeed();
    notifyListeners();
  }

  void retry(DesktopFileTransfer job) {
    if (!job.finished || job.state == 'complete') return;
    if (jobs.any(
      (other) =>
          !other.finished &&
          other.machine.id == job.machine.id &&
          other.service == job.service &&
          other.workspace == job.workspace &&
          other.path == job.path,
    )) {
      return;
    }
    enqueue(
      machine: job.machine,
      incarnation: job.incarnation,
      service: job.service,
      workspace: job.workspace,
      paths: [job.path],
      names: {job.path: job.name},
      sizes: {job.path: job.sourceSizeBytes},
      resume: job.errorCode != 'TRANSFER_EXPIRED',
    );
  }

  @override
  void dispose() {
    _disposed = true;
    _speedTimer?.cancel();
    _wakeUp();
    super.dispose();
  }
}

class FileDropArea extends StatefulWidget {
  const FileDropArea({
    super.key,
    required this.onFiles,
    required this.child,
    this.enabled = true,
  });
  final Future<void> Function(List<String>) onFiles;
  final Widget child;
  final bool enabled;
  @override
  State<FileDropArea> createState() => _FileDropAreaState();
}

class _FileDropAreaState extends State<FileDropArea> {
  bool hovering = false;

  Future<void> dropped(DropDetails details) async {
    try {
      final files = details.items.whereType<DropFileItem>().toList();
      if (files.isEmpty ||
          details.items.any((item) => item is DropDirectoryItem)) {
        throw const FormatException(
          'Drop files here. To send a folder, create an archive first.',
        );
      }
      final paths = <String>{};
      for (final file in files) {
        final path = file.localPath;
        if (path == null || file.isPromised) {
          throw const FormatException(
            'Save these files to your computer before sending them.',
          );
        }
        if (await FileSystemEntity.type(path, followLinks: false) !=
            FileSystemEntityType.file) {
          throw const FormatException(
            'Choose regular files. Folders and shortcuts cannot be sent.',
          );
        }
        paths.add(path);
      }
      if (!mounted || !widget.enabled) return;
      // Keep the native drop's provider access alive until the selection is
      // dismissed or its submitted transfers finish, including while hidden.
      await widget.onFiles(paths.toList());
    } catch (error) {
      if (!mounted) return;
      ScaffoldMessenger.of(context)
          .showSnackBar(SnackBar(content: Text(failureMessage(error))));
    }
  }

  @override
  Widget build(BuildContext context) => DropTarget(
    enabled: widget.enabled && (ModalRoute.of(context)?.isCurrent ?? true),
    acceptedKinds: const {DropItemKind.file},
    onDrop: dropped,
    onEnter: (_) => setState(() => hovering = true),
    onLeave: () => setState(() => hovering = false),
    child: Stack(
      children: [
        widget.child,
        if (hovering && widget.enabled)
          Positioned.fill(
            child: IgnorePointer(
              child: Container(
                decoration: BoxDecoration(
                  color: const Color(0xe60b1528),
                  borderRadius: BorderRadius.circular(16),
                  border: Border.all(color: WispColors.cyan, width: 2),
                ),
                alignment: Alignment.center,
                child: const Padding(
                  padding: EdgeInsets.all(8),
                  child: Text(
                    'Drop files to send',
                    textAlign: TextAlign.center,
                  ),
                ),
              ),
            ),
          ),
      ],
    ),
  );
}

class _SelectedFile {
  _SelectedFile(this.path, {String? name, this.sizeBytes = 0})
    : name = name ?? _fileName(path);
  final String path;
  final String name;
  final int sizeBytes;
  final completion = Completer<void>();
  void release() {
    if (!completion.isCompleted) completion.complete();
  }
}

class FileSendCommand extends StatelessWidget {
  const FileSendCommand({super.key, required this.host, required this.service});
  final String host, service;

  @override
  Widget build(BuildContext context) {
    final prefix =
        "wispnet files put $host ${service == 'files' ? "''" : service}";
    return WispCommandBlock(
      command: '$prefix /local/file filename',
      copyText: prefix,
      copyTooltip: 'Copy send command',
      copyMessage: 'Command prefix copied',
    );
  }
}

class FileSendSheet extends StatefulWidget {
  const FileSendSheet({
    super.key,
    required this.backend,
    required this.machine,
    required this.incarnation,
    required this.transfers,
    this.service = 'files',
    this.initialPaths = const [],
    this.initialFiles = const [],
    this.onQueued,
    this.desktop = true,
  });
  final WispnetBackend backend;
  final Machine machine;
  final String incarnation, service;
  final DesktopFileTransfers transfers;
  final List<String> initialPaths;
  final List<Map<String, dynamic>> initialFiles;
  final ValueChanged<List<DesktopFileTransfer>>? onQueued;
  final bool desktop;
  @override
  State<FileSendSheet> createState() => _FileSendSheetState();
}

class _FileSendSheetState extends State<FileSendSheet> {
  final List<_SelectedFile> selected = [];
  bool picking = false;
  String? feedback;

  @override
  void initState() {
    super.initState();
    selected.addAll(widget.initialPaths.map(_SelectedFile.new));
    addSelection(widget.initialFiles);
  }

  Future<void> addFiles(List<String> paths) async {
    final existing = selected.map((file) => file.path).toSet();
    final added = paths.where(existing.add).map(_SelectedFile.new).toList();
    setState(() {
      selected.addAll(added);
      feedback = null;
    });
    await Future.wait(added.map((file) => file.completion.future));
  }

  void addSelection(List<dynamic> entries) {
    final existing = selected.map((file) => file.path).toSet();
    final added = <_SelectedFile>[];
    for (final entry in entries) {
      if (entry is! Map) {
        throw const FormatException('Could not read the selected files.');
      }
      final path = entry['path'] as String?;
      final name = entry['name'] as String?;
      if (path == null ||
          Uri.tryParse(path)?.scheme != 'content' ||
          name == null ||
          name.isEmpty ||
          name == '.' ||
          name == '..' ||
          name.contains('/') ||
          name.contains('\\') ||
          name.contains('\u0000')) {
        throw const FormatException(
          'Could not read a selected file name. Choose another file.',
        );
      }
      if (existing.add(path)) {
        added.add(
          _SelectedFile(
            path,
            name: name,
            sizeBytes: (entry['size_bytes'] as num?)?.toInt() ?? 0,
          ),
        );
      }
    }
    selected.addAll(added);
    if (added.isNotEmpty) feedback = null;
  }

  Future<void> pickFiles() async {
    if (picking) return;
    setState(() => picking = true);
    try {
      final response = await widget.backend.invoke('pickFiles');
      if (!mounted) return;
      setState(() => addSelection(response['files'] as List? ?? const []));
    } catch (error) {
      if (mounted) setState(() => feedback = failureMessage(error));
    } finally {
      if (mounted) setState(() => picking = false);
    }
  }

  void send() {
    if (selected.isEmpty || picking) return;
    final names = selected.map((file) => file.name).toList();
    if (names.toSet().length != names.length) {
      setState(
        () => feedback = 'Two files have the same name. Send them separately or rename one first.',
      );
      return;
    }
    final pending = selected.toList();
    final jobs = widget.transfers.enqueue(
      machine: widget.machine,
      incarnation: widget.incarnation,
      service: widget.service,
      paths: pending.map((file) => file.path).toList(),
      names: {for (final file in pending) file.path: file.name},
      sizes: {for (final file in pending) file.path: file.sizeBytes},
    );
    for (var i = 0; i < jobs.length; i++) {
      unawaited(jobs[i].done.then((_) => pending[i].release()));
    }
    widget.onQueued?.call(jobs);
    setState(() {
      selected.clear();
      feedback = null;
    });
  }

  @override
  void dispose() {
    for (final file in selected) {
      file.release();
    }
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => Column(
    crossAxisAlignment: CrossAxisAlignment.stretch,
    children: [
      if (widget.service != 'files')
        Padding(
          padding: const EdgeInsets.only(bottom: 12),
          child: Text('Inbox: ${widget.service}'),
        ),
      if (widget.desktop)
        FileDropArea(
          key: const ValueKey('file-drop-zone'),
          onFiles: addFiles,
          child: Container(
            width: double.infinity,
            constraints: const BoxConstraints(minHeight: 112),
            padding: const EdgeInsets.all(18),
            decoration: BoxDecoration(
              color: WispColors.tile,
              borderRadius: BorderRadius.circular(16),
              border: Border.all(color: const Color(0xff32518a)),
            ),
            child: const Column(
              children: [
                Icon(Icons.upload_file, color: WispColors.cyan),
                SizedBox(height: 8),
                Text('Drop files here'),
              ],
            ),
          ),
        )
      else
        OutlinedButton.icon(
          key: const ValueKey('file-pick'),
          onPressed: picking ? null : pickFiles,
          icon: picking
              ? const SizedBox(
                  width: 18,
                  height: 18,
                  child: CircularProgressIndicator(strokeWidth: 2),
                )
              : const Icon(Icons.upload_file),
          label: Text(
            picking
                ? 'Choosing files…'
                : selected.isEmpty
                ? 'Choose files'
                : 'Add files',
          ),
        ),
      for (final file in selected)
        WispListItem(
          contentPadding: EdgeInsets.zero,
          leading: const Icon(Icons.insert_drive_file_outlined),
          title: Text(file.name, maxLines: 2, overflow: TextOverflow.ellipsis),
          actions: [
            IconButton(
              tooltip: 'Remove ${file.name}',
              onPressed: () => setState(() {
                selected.remove(file);
                file.release();
              }),
              icon: const Icon(Icons.close),
            ),
          ],
        ),
      if (feedback != null) ...[
        const SizedBox(height: 12),
        Text(feedback!, style: const TextStyle(color: WispColors.amber)),
      ],
      const SizedBox(height: 12),
      FilledButton.icon(
        key: const ValueKey('file-send'),
        onPressed: selected.isEmpty || picking ? null : send,
        icon: const Icon(Icons.send),
        label: Text(
          selected.length > 1 ? 'Send ${selected.length} files' : 'Send files',
        ),
      ),
      const SizedBox(height: 16),
      FileSendCommand(host: widget.machine.id, service: widget.service),
      const SizedBox(height: 16),
      FileTransferList(transfers: widget.transfers, host: widget.machine.id),
    ],
  );
}

class FileTransferList extends StatelessWidget {
  const FileTransferList({super.key, required this.transfers, this.host});
  final DesktopFileTransfers transfers;
  final String? host;

  double? progress(DesktopFileTransfer job) {
    if (job.sizeBytes <= 0 ||
        const ['preparing', 'hashing', 'queued'].contains(job.phase) ||
        const ['queued', 'starting'].contains(job.state)) {
      return null;
    }
    final bytes = job.phase == 'copying' ? job.copiedBytes : job.verifiedBytes;
    return (bytes / job.sizeBytes).clamp(0, 1);
  }

  String amount(DesktopFileTransfer job) {
    final value = progress(job);
    final bytes = job.phase == 'copying' ? job.copiedBytes : job.verifiedBytes;
    if (value == null) return '${fileSize(bytes)} sent';
    return '${(value * 100).floor()}% · ${fileSize(bytes)} / ${fileSize(job.sizeBytes)}';
  }

  String status(DesktopFileTransfer job) {
    if (job.cancelling) return 'Cancelling…';
    if (job.state == 'complete') return 'Sent · ${fileSize(job.sizeBytes)}';
    if (job.state == 'cancelled') return 'Cancelled · you can resume this file';
    if (job.state == 'failed') return job.error ?? 'Transfer failed';
    if (job.state == 'queued' || job.phase == 'queued') return 'Queued';
    if (job.error != null) return job.error!;
    if (job.phase == 'copying') {
      return job.sizeBytes > 0
          ? 'Preparing file · ${amount(job)}'
          : 'Preparing file · ${fileSize(job.copiedBytes)}';
    }
    if (job.phase == 'hashing' || job.phase == 'preparing') {
      return 'Preparing file…';
    }
    if (job.phase == 'verifying') return 'Verifying file…';
    if (job.phase == 'reconnecting') return 'Reconnecting…';
    return amount(job);
  }

  @override
  Widget build(BuildContext context) => ListenableBuilder(
    listenable: transfers,
    builder: (context, _) => Column(
      key: const ValueKey('file-transfer-list'),
      crossAxisAlignment: CrossAxisAlignment.stretch,
      children: [
        for (final job in transfers.jobs.where(
          (job) => host == null || job.machine.id == host,
        ))
          Padding(
            padding: const EdgeInsets.symmetric(vertical: 8),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                Row(
                  children: [
                    Expanded(
                      child: Column(
                        crossAxisAlignment: CrossAxisAlignment.start,
                        children: [
                          Text(
                            job.name,
                            maxLines: 2,
                            overflow: TextOverflow.ellipsis,
                          ),
                          Text(
                            job.service == 'files'
                                ? job.hostName
                                : '${job.hostName} / ${job.service}',
                            style: const TextStyle(
                              color: WispColors.muted,
                              fontSize: 12,
                            ),
                          ),
                          Text(
                            status(job),
                            style: TextStyle(
                              color: job.state == 'failed'
                                  ? WispColors.error
                                  : WispColors.muted,
                            ),
                          ),
                          if (job.uploading)
                            Text(
                              job.bytesPerSecond == null
                                  ? 'Measuring upload speed…'
                                  : '${fileSize(job.bytesPerSecond!.round())}/s',
                              style: const TextStyle(
                                color: WispColors.cyan,
                                fontWeight: FontWeight.w600,
                              ),
                            ),
                        ],
                      ),
                    ),
                    if (!job.finished)
                      IconButton(
                        tooltip: 'Cancel ${job.name}',
                        onPressed: job.cancelling
                            ? null
                            : () => transfers.cancel(job),
                        icon: const Icon(Icons.close),
                      )
                    else if (job.state != 'complete')
                      IconButton(
                        tooltip: job.errorCode == 'TRANSFER_EXPIRED'
                            ? 'Send ${job.name} again'
                            : 'Resume ${job.name}',
                        onPressed: () => transfers.retry(job),
                        icon: const Icon(Icons.refresh),
                      )
                    else
                      const Padding(
                        padding: EdgeInsets.all(12),
                        child: Icon(Icons.check, color: WispColors.green),
                      ),
                  ],
                ),
                if (!job.finished) ...[
                  const SizedBox(height: 8),
                  LinearProgressIndicator(
                    value: progress(job),
                    minHeight: 10,
                    borderRadius: BorderRadius.circular(6),
                    color: WispColors.cyan,
                    backgroundColor: const Color(0xff263a59),
                    semanticsLabel: 'Upload progress for ${job.name}',
                    semanticsValue: progress(job) == null
                        ? status(job)
                        : '${(progress(job)! * 100).floor()}%',
                  ),
                ],
              ],
            ),
          ),
      ],
    ),
  );
}
