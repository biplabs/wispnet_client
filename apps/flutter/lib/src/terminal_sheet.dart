import 'dart:async';
import 'dart:convert';
import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/gestures.dart';

import 'backend.dart';
import '../ui.dart';

enum DesktopInputMode {
  auto('Auto'),
  predictive('Predictive'),
  sync('Sync'),
  line('Line');

  const DesktopInputMode(this.label);
  final String label;

  String get summary => switch (this) {
    DesktopInputMode.auto => 'Adapts to latency: Sync ≤80 ms, Predictive ≤220 ms, Line on slower paths.',
    DesktopInputMode.predictive =>
      'Shows typed characters immediately and reconciles remote echoes.',
    DesktopInputMode.sync =>
      'Waits for the host to echo each keystroke before displaying it.',
    DesktopInputMode.line =>
      'Compose a full command locally before sending it all at once.',
  };

  DesktopInputMode effective(int? latencyMs) {
    if (this != DesktopInputMode.auto) return this;
    if (latencyMs == null) return DesktopInputMode.predictive;
    if (latencyMs <= 80) return DesktopInputMode.sync;
    if (latencyMs <= 220) return DesktopInputMode.predictive;
    return DesktopInputMode.line;
  }

  DesktopInputMode resolve({
    required int? latencyMs,
    required bool echo,
    required bool canonical,
  }) => (!echo || !canonical) ? DesktopInputMode.sync : effective(latencyMs);
}

enum ShellPaneState { connecting, attached, disconnected, exited, failed }

class TerminalCell {
  TerminalCell({
    this.char = ' ',
    this.fg = const Color(0xFFEDF2FF),
    this.bg = Colors.transparent,
    this.bold = false,
  });
  String char;
  Color fg;
  Color bg;
  bool bold;

  TerminalCell copy() => TerminalCell(char: char, fg: fg, bg: bg, bold: bold);
}

/// Lightweight VT100/ANSI screen buffer for the baked-in desktop terminal.
class _TerminalTextSink implements Sink<String> {
  _TerminalTextSink(this.onText);
  final void Function(String) onText;

  @override
  void add(String data) => onText(data);

  @override
  void close() {}
}

class TerminalEmulatorBuffer {
  TerminalEmulatorBuffer({int cols = 80, int rows = 24, this.onReply})
    : _cols = cols.clamp(2, 500),
      _rows = rows.clamp(2, 300) {
    reset();
  }

  int _cols;
  int _rows;
  int get cols => _cols;
  int get rows => _rows;
  void Function(String)? onReply;
  int version = 0;
  late ByteConversionSink _decoder;
  String _pendingEscape = '';

  late List<List<TerminalCell>> _grid;
  final List<List<TerminalCell>> _scrollback = [];
  int cursorRow = 0;
  int cursorCol = 0;
  int _savedRow = 0;
  int _savedCol = 0;
  Color _fg = const Color(0xFFEDF2FF);
  Color _bg = Colors.transparent;
  bool _bold = false;
  bool _inverse = false;
  String? lastTitle;

  static const _defaultFg = Color(0xFFEDF2FF);
  static const _ansiColors = <Color>[
    Color(0xFF1B2234),
    Color(0xFFFF6B7B),
    Color(0xFF7BE0B8),
    Color(0xFFF0B85C),
    Color(0xFF65CEFF),
    Color(0xFFC792EA),
    Color(0xFF89DDFF),
    Color(0xFFEDF2FF),
    Color(0xFF5C6E8C),
    Color(0xFFFF9AA8),
    Color(0xFF9DF0CE),
    Color(0xFFFFCB6B),
    Color(0xFF82AAFF),
    Color(0xFFD6ACFF),
    Color(0xFFA6F0FF),
    Color(0xFFFFFFFF),
  ];

  List<TerminalCell> _blankRow([int? width]) =>
      List.generate(width ?? _cols, (_) => TerminalCell());

  void reset() {
    _pendingEscape = '';
    _decoder = const Utf8Decoder(allowMalformed: true)
        .startChunkedConversion(_TerminalTextSink(writeString));
    _grid = List.generate(_rows, (_) => _blankRow());
    _scrollback.clear();
    cursorRow = 0;
    cursorCol = 0;
    _savedRow = 0;
    _savedCol = 0;
    _fg = _defaultFg;
    _bg = Colors.transparent;
    _bold = false;
    _inverse = false;
    version++;
  }

  void resize(int nextCols, int nextRows) {
    final c = nextCols.clamp(2, 500);
    final r = nextRows.clamp(2, 300);
    if (c == _cols && r == _rows) return;
    final nextGrid = List.generate(r, (rowIdx) {
      if (rowIdx < _grid.length) {
        final existing = _grid[rowIdx];
        return List.generate(
          c,
          (colIdx) =>
              colIdx < existing.length ? existing[colIdx] : TerminalCell(),
        );
      }
      return _blankRow(c);
    });
    _cols = c;
    _rows = r;
    _grid = nextGrid;
    cursorRow = cursorRow.clamp(0, _rows - 1);
    cursorCol = cursorCol.clamp(0, _cols - 1);
    version++;
  }

  void writeBytes(List<int> bytes) {
    _decoder.add(bytes);
  }

  void writeString(String text) {
    text = _pendingEscape + text;
    _pendingEscape = '';
    var i = 0;
    while (i < text.length) {
      final code = text.codeUnitAt(i);
      if (code == 0x1B || code == 0x9B) {
        final next = code == 0x1B
            ? _parseEscape(text, i + 1)
            : _parseCsiBody(text, i + 1);
        if (next < 0) {
          _pendingEscape = text.substring(i);
          break;
        }
        i = next;
        continue;
      }
      if (code == 0x0D) {
        cursorCol = 0;
      } else if (code == 0x0A || code == 0x0B || code == 0x0C) {
        _lineFeed();
      } else if (code == 0x08 || code == 0x7F) {
        if (cursorCol > 0) {
          cursorCol--;
          if (code == 0x7F) {
            _grid[cursorRow][cursorCol] = TerminalCell();
          }
        }
      } else if (code == 0x09) {
        cursorCol = ((cursorCol ~/ 8) + 1) * 8;
        if (cursorCol >= _cols) {
          cursorCol = _cols - 1;
        }
      } else if (code >= 0x20) {
        if (code >= 0xD800 && code <= 0xDBFF && i + 1 < text.length) {
          final low = text.codeUnitAt(i + 1);
          if (low >= 0xDC00 && low <= 0xDFFF) {
            _putChar(text.substring(i, i + 2));
            i += 2;
            continue;
          }
        }
        _putChar(text[i]);
      }
      i++;
    }
    version++;
  }

  void _putChar(String ch) {
    if (cursorCol >= _cols) {
      cursorCol = 0;
      _lineFeed();
    }
    final fg = _inverse
        ? (_bg == Colors.transparent ? Colors.black : _bg)
        : _fg;
    final bg = _inverse ? _fg : _bg;
    _grid[cursorRow][cursorCol] = TerminalCell(
      char: ch,
      fg: fg,
      bg: bg,
      bold: _bold,
    );
    cursorCol++;
  }

  void _lineFeed() {
    if (cursorRow + 1 < _rows) {
      cursorRow++;
    } else {
      _scrollback.add(_grid.removeAt(0));
      if (_scrollback.length > 400) {
        _scrollback.removeAt(0);
      }
      _grid.add(_blankRow());
    }
  }

  void _reverseLineFeed() {
    if (cursorRow > 0) {
      cursorRow--;
    } else {
      _grid.removeLast();
      _grid.insert(0, _blankRow());
    }
  }

  int _parseEscape(String text, int start) {
    if (start >= text.length) return -1;
    final prefix = text[start];
    if (prefix == '[') {
      return _parseCsiBody(text, start + 1);
    }
    if (prefix == ']') {
      // OSC sequence: ESC ] ... (BEL | ESC \ | ST)
      return _parseControlString(text, start + 1, onComplete: _handleOsc);
    }
    if (prefix == 'P') {
      // DCS sequence: ESC P ... (BEL | ESC \ | ST)
      return _parseControlString(text, start + 1, onComplete: _handleDcs);
    }
    if (prefix == 'X' || prefix == '^' || prefix == '_' || prefix == 'k') {
      // SOS, PM, APC, or screen title string: consume until ST/BEL
      return _parseControlString(text, start + 1);
    }
    if (prefix == '(' ||
        prefix == ')' ||
        prefix == '*' ||
        prefix == '+' ||
        prefix == '-' ||
        prefix == '.' ||
        prefix == '/' ||
        prefix == '#') {
      // 2-byte character set / line attribute sequence (e.g. ESC ( B)
      return start + 2 <= text.length ? start + 2 : -1;
    }
    if (prefix == '7') {
      _savedRow = cursorRow;
      _savedCol = cursorCol;
      return start + 1;
    }
    if (prefix == '8') {
      cursorRow = _savedRow.clamp(0, _rows - 1);
      cursorCol = _savedCol.clamp(0, _cols - 1);
      return start + 1;
    }
    if (prefix == 'D') {
      _lineFeed();
      return start + 1;
    }
    if (prefix == 'E') {
      cursorCol = 0;
      _lineFeed();
      return start + 1;
    }
    if (prefix == 'M') {
      _reverseLineFeed();
      return start + 1;
    }
    if (prefix == 'c') {
      reset();
      return start + 1;
    }
    // Includes ESC =, ESC >, ESC \, etc.
    return start + 1;
  }

  int _parseCsiBody(String text, int idx) {
    final params = StringBuffer();
    while (idx < text.length) {
      final cu = text.codeUnitAt(idx);
      if (cu >= 0x40 && cu <= 0x7E) {
        _handleCsi(params.toString(), text[idx]);
        return idx + 1;
      }
      params.write(text[idx]);
      idx++;
    }
    return -1;
  }

  int _parseControlString(
    String text,
    int idx, {
    void Function(String)? onComplete,
  }) {
    final body = StringBuffer();
    while (idx < text.length) {
      final cu = text.codeUnitAt(idx);
      if (cu == 0x07 || cu == 0x9C) {
        onComplete?.call(body.toString());
        return idx + 1;
      }
      if (cu == 0x1B && idx + 1 < text.length && text[idx + 1] == '\\') {
        onComplete?.call(body.toString());
        return idx + 2;
      }
      body.write(text[idx]);
      idx++;
    }
    return -1;
  }

  void _handleDcs(String content) {
    if (content.startsWith('+q')) {
      // XTGETTCAP query: reply with "0+r" (capability not recognized) so
      // shells like fish don't wait or retry.
      onReply?.call('\x1bP0+r\x1b\\');
    }
  }

  void _handleOsc(String content) {
    final semi = content.indexOf(';');
    if (semi < 0) return;
    final code = content.substring(0, semi);
    final payload = content.substring(semi + 1).trim();
    if (code == '0' || code == '2') {
      if (payload.isNotEmpty) {
        final cleaned = payload.split(RegExp(r'[/\\]')).last.trim();
        lastTitle = cleaned.isNotEmpty ? cleaned : payload;
      }
    } else if (code == '10' && payload == '?') {
      onReply?.call('\x1b]10;rgb:edef/f2f2/ffff\x1b\\');
    } else if (code == '11' && payload == '?') {
      onReply?.call('\x1b]11;rgb:0000/0000/0000\x1b\\');
    }
  }

  void _handleCsi(String rawParams, String command) {
    // Handle Device Attributes & Device Status Report queries first.
    if (command == 'c') {
      if (rawParams.startsWith('>')) {
        onReply?.call('\x1b[>0;10;1c');
      } else if (!rawParams.startsWith('?') && !rawParams.startsWith('=')) {
        onReply?.call('\x1b[?1;2c');
      }
      return;
    }
    if (command == 'n' &&
        !rawParams.startsWith('?') &&
        !rawParams.startsWith('>')) {
      if (rawParams == '5') {
        onReply?.call('\x1b[0n');
      } else if (rawParams == '6') {
        onReply?.call('\x1b[${cursorRow + 1};${cursorCol + 1}R');
      }
      return;
    }

    // Ignore private-mode or intermediate CSI sequences (e.g. CSI ? u, CSI > 4;1 m, CSI 2 q)
    // except DECSET/DECRST alternate screen buffer clear (?1049h / ?47h).
    if (rawParams.isNotEmpty) {
      final first = rawParams.codeUnitAt(0);
      if (first == 0x3F || first == 0x3E || first == 0x3D || first == 0x3C) {
        // '?' (0x3F), '>' (0x3E), '=' (0x3D), '<' (0x3C)
        if (first == 0x3F && (command == 'h' || command == 'l')) {
          final modeStr = rawParams.substring(1);
          if ((modeStr == '1049' || modeStr == '47' || modeStr == '1047') &&
              command == 'h') {
            for (var r = 0; r < _rows; r++) {
              _grid[r] = _blankRow();
            }
            cursorRow = 0;
            cursorCol = 0;
          }
        }
        return;
      }
      // Check for intermediate bytes (0x20..0x2F, such as space in `CSI 2 q`)
      for (var i = 0; i < rawParams.length; i++) {
        final cu = rawParams.codeUnitAt(i);
        if (cu >= 0x20 && cu <= 0x2F) {
          return;
        }
      }
    }

    final normalizedParams = rawParams.replaceAll(':', ';');
    final parts = normalizedParams.isEmpty
        ? <int>[]
        : normalizedParams.split(';').map((s) => int.tryParse(s) ?? 0).toList();
    int param(int index, int fallback) =>
        (index < parts.length && parts[index] > 0) ? parts[index] : fallback;

    switch (command) {
      case 'A':
        cursorRow = (cursorRow - param(0, 1)).clamp(0, _rows - 1);
      case 'B':
        cursorRow = (cursorRow + param(0, 1)).clamp(0, _rows - 1);
      case 'C':
        cursorCol = (cursorCol + param(0, 1)).clamp(0, _cols - 1);
      case 'D':
        cursorCol = (cursorCol - param(0, 1)).clamp(0, _cols - 1);
      case 'E':
        cursorRow = (cursorRow + param(0, 1)).clamp(0, _rows - 1);
        cursorCol = 0;
      case 'F':
        cursorRow = (cursorRow - param(0, 1)).clamp(0, _rows - 1);
        cursorCol = 0;
      case 'G' || '`':
        cursorCol = (param(0, 1) - 1).clamp(0, _cols - 1);
      case 'd':
        cursorRow = (param(0, 1) - 1).clamp(0, _rows - 1);
      case 'H' || 'f':
        cursorRow = (param(0, 1) - 1).clamp(0, _rows - 1);
        cursorCol = (param(1, 1) - 1).clamp(0, _cols - 1);
      case 'J':
        final mode = parts.isEmpty ? 0 : parts[0];
        if (mode == 2 || mode == 3) {
          for (var r = 0; r < _rows; r++) {
            _grid[r] = _blankRow();
          }
          cursorRow = 0;
          cursorCol = 0;
        } else if (mode == 0) {
          for (var c = cursorCol; c < _cols; c++) {
            _grid[cursorRow][c] = TerminalCell();
          }
          for (var r = cursorRow + 1; r < _rows; r++) {
            _grid[r] = _blankRow();
          }
        } else if (mode == 1) {
          for (var r = 0; r < cursorRow; r++) {
            _grid[r] = _blankRow();
          }
          for (var c = 0; c <= cursorCol && c < _cols; c++) {
            _grid[cursorRow][c] = TerminalCell();
          }
        }
      case 'K':
        final mode = parts.isEmpty ? 0 : parts[0];
        if (mode == 0) {
          for (var c = cursorCol; c < _cols; c++) {
            _grid[cursorRow][c] = TerminalCell();
          }
        } else if (mode == 1) {
          for (var c = 0; c <= cursorCol && c < _cols; c++) {
            _grid[cursorRow][c] = TerminalCell();
          }
        } else if (mode == 2) {
          _grid[cursorRow] = _blankRow();
        }
      case 'X':
        final count = param(0, 1);
        for (var c = cursorCol; c < _cols && c < cursorCol + count; c++) {
          _grid[cursorRow][c] = TerminalCell();
        }
      case '@':
        final count = param(0, 1);
        final row = _grid[cursorRow];
        for (var c = _cols - 1; c >= cursorCol; c--) {
          row[c] = (c - count >= cursorCol) ? row[c - count] : TerminalCell();
        }
      case 'P':
        final count = param(0, 1);
        final row = _grid[cursorRow];
        for (var c = cursorCol; c < _cols; c++) {
          row[c] = (c + count < _cols) ? row[c + count] : TerminalCell();
        }
      case 'L':
        final count = param(0, 1).clamp(1, _rows - cursorRow);
        for (var i = 0; i < count; i++) {
          _grid.removeLast();
          _grid.insert(cursorRow, _blankRow());
        }
      case 'M':
        final count = param(0, 1).clamp(1, _rows - cursorRow);
        for (var i = 0; i < count; i++) {
          _grid.removeAt(cursorRow);
          _grid.add(_blankRow());
        }
      case 's':
        _savedRow = cursorRow;
        _savedCol = cursorCol;
      case 'u':
        cursorRow = _savedRow.clamp(0, _rows - 1);
        cursorCol = _savedCol.clamp(0, _cols - 1);
      case 'm':
        _handleSgr(parts.isEmpty ? const [0] : parts);
    }
  }

  void _handleSgr(List<int> codes) {
    var i = 0;
    while (i < codes.length) {
      final code = codes[i];
      if (code == 0) {
        _fg = _defaultFg;
        _bg = Colors.transparent;
        _bold = false;
        _inverse = false;
      } else if (code == 1) {
        _bold = true;
      } else if (code == 22) {
        _bold = false;
      } else if (code == 7) {
        _inverse = true;
      } else if (code == 27) {
        _inverse = false;
      } else if (code >= 30 && code <= 37) {
        _fg = _ansiColors[code - 30 + (_bold ? 8 : 0)];
      } else if (code == 39) {
        _fg = _defaultFg;
      } else if (code >= 40 && code <= 47) {
        _bg = _ansiColors[code - 40];
      } else if (code == 49) {
        _bg = Colors.transparent;
      } else if (code >= 90 && code <= 97) {
        _fg = _ansiColors[code - 90 + 8];
      } else if (code >= 100 && code <= 107) {
        _bg = _ansiColors[code - 100 + 8];
      } else if ((code == 38 || code == 48) && i + 2 < codes.length) {
        final isFg = code == 38;
        final mode = codes[i + 1];
        if (mode == 5) {
          final color = _color256(codes[i + 2]);
          if (isFg) {
            _fg = color;
          } else {
            _bg = color;
          }
          i += 3;
          continue;
        } else if (mode == 2 && i + 4 < codes.length) {
          final color = Color.fromARGB(
            255,
            codes[i + 2].clamp(0, 255),
            codes[i + 3].clamp(0, 255),
            codes[i + 4].clamp(0, 255),
          );
          if (isFg) {
            _fg = color;
          } else {
            _bg = color;
          }
          i += 5;
          continue;
        }
      }
      i++;
    }
  }

  Color _color256(int idx) {
    final n = idx.clamp(0, 255);
    if (n < 16) return _ansiColors[n];
    if (n < 232) {
      final v = n - 16;
      final r = (v ~/ 36) % 6;
      final g = (v ~/ 6) % 6;
      final b = v % 6;
      int ch(int x) => x == 0 ? 0 : 55 + x * 40;
      return Color.fromARGB(255, ch(r), ch(g), ch(b));
    }
    final gray = 8 + (n - 232) * 10;
    return Color.fromARGB(255, gray, gray, gray);
  }

  List<List<TerminalCell>> get lines => [..._scrollback, ..._grid];
  int get activeStartLine => _scrollback.length;
}

class DesktopShellPane extends ChangeNotifier {
  DesktopShellPane({
    required this.backend,
    required this.host,
    required this.hostName,
    required this.service,
    required this.incarnation,
    required this.session,
    this.android = false,
  }) {
    buffer.onReply = (reply) => unawaited(sendInput(reply));
  }

  final WispnetBackend backend;
  final String host;
  final String hostName;
  final String service;
  final String incarnation;
  final String session;
  final bool android;

  Map<String, dynamic> get nativeArguments => {
    'host': host,
    'host_name': hostName,
    'service': service,
    'incarnation': incarnation,
    'session': session,
  };

  final TerminalEmulatorBuffer buffer = TerminalEmulatorBuffer();
  final FocusNode focusNode = FocusNode();
  ShellPaneState state = ShellPaneState.connecting;
  String? errorMessage;
  String? handle;
  String path = 'direct';
  int? latencyMs;
  bool echoEnabled = true;
  bool canonical = true;
  String? title;
  String lineDraft = '';
  bool _closed = false;
  bool _terminateOnClose = false;
  Timer? _pollTimer;
  bool _polling = false;
  bool _replayStarted = false;
  final List<int> _pendingEcho = [];

  Future<void> connect() async {
    if (_closed) return;
    _pollTimer?.cancel();
    state = ShellPaneState.connecting;
    errorMessage = null;
    _replayStarted = false;
    notifyListeners();
    try {
      if (android) {
        handle = session;
        await backend.invoke('nativeTerminalOpen', nativeArguments);
        if (_closed) return;
        final reply = await backend.invoke(
          'nativeTerminalReconnect',
          nativeArguments,
        );
        if (_closed) return;
        _consumeNative(reply);
        _pollTimer = Timer.periodic(
          const Duration(milliseconds: 100),
          (_) => unawaited(_pollOnce()),
        );
        return;
      }
      final reply = await backend.invoke('shellOpen', {
        'host': host,
        'service': service,
        'incarnation': incarnation,
        'session': session,
        'cols': buffer.cols,
        'rows': buffer.rows,
      });
      final opened = reply['session'] as String?;
      if (_closed) {
        if (opened != null) {
          unawaited(
            backend.invoke('shellClose', {
              'session': opened,
              'terminate': _terminateOnClose,
            }),
          );
        }
        return;
      }
      handle = opened;
      state = ShellPaneState.attached;
      _consume(reply);
      _pollTimer = Timer.periodic(
        const Duration(milliseconds: 30),
        (_) => unawaited(_pollOnce()),
      );
      notifyListeners();
    } catch (error) {
      if (_closed) return;
      state = ShellPaneState.failed;
      errorMessage = error is PlatformException
          ? (error.message ?? error.code)
          : error.toString();
      notifyListeners();
    }
  }

  Future<void> _pollOnce() async {
    if (_closed || _polling || handle == null) return;
    _polling = true;
    try {
      final reply = await backend.invoke(
        android ? 'nativeTerminalStatus' : 'shellPoll',
        android ? nativeArguments : {'session': handle},
      );
      if (_closed) return;
      if (android) {
        _consumeNative(reply);
      } else {
        _consume(reply);
      }
    } catch (error) {
      if (_closed) return;
      _pollTimer?.cancel();
      handle = null;
      state = ShellPaneState.disconnected;
      errorMessage = error is PlatformException
          ? (error.message ?? error.code)
          : error.toString();
      notifyListeners();
    } finally {
      _polling = false;
    }
  }

  void _consumeNative(Map<String, dynamic> reply) {
    final previous = (
      state,
      errorMessage,
      latencyMs,
      path,
      title,
      echoEnabled,
      canonical,
    );
    state = ShellPaneState.values.firstWhere(
      (value) => value.name == reply['state'],
      orElse: () => ShellPaneState.connecting,
    );
    errorMessage = reply['error'] as String?;
    latencyMs = reply['latency_ms'] as int?;
    path = reply['path'] as String? ?? 'unknown';
    title = reply['title'] as String?;
    echoEnabled = reply['echo_enabled'] == true;
    canonical = reply['canonical'] == true;
    if (previous !=
        (state, errorMessage, latencyMs, path, title, echoEnabled, canonical)) {
      notifyListeners();
    }
  }

  void _consume(Map<String, dynamic> reply) {
    final nextLatency = reply['latency_ms'];
    if (nextLatency is int && nextLatency >= 0) {
      latencyMs = nextLatency;
    }
    final nextPath = reply['path'];
    if (nextPath is String && nextPath.isNotEmpty) {
      path = nextPath;
    }
    if (reply.containsKey('echo_enabled')) {
      echoEnabled = reply['echo_enabled'] == true;
      canonical = reply['canonical'] == true;
      if (!echoEnabled || !canonical) {
        _pendingEcho.clear();
      }
    }
    final events = (reply['events'] as List?) ?? const [];
    for (final raw in events) {
      if (raw is! Map) continue;
      final event = raw.cast<String, dynamic>();
      final type = event['type'] as String? ?? '';
      if (type == 'output' || type == 'scrollback') {
        final data = base64Decode(event['data_b64'] as String? ?? '');
        if (type == 'scrollback') {
          if (!_replayStarted) {
            buffer.reset();
            _replayStarted = true;
          }
          _pendingEcho.clear();
          buffer.writeBytes(data);
        } else {
          final filtered = _reconcileEcho(data);
          if (filtered.isNotEmpty) {
            buffer.writeBytes(filtered);
          }
        }
        if (buffer.lastTitle != null && buffer.lastTitle != title) {
          title = buffer.lastTitle;
        }
      } else if (type == 'terminal_state') {
        echoEnabled = event['echo'] == true && echoEnabled;
        canonical = event['canonical'] == true && canonical;
      } else if (type == 'exit') {
        final code = event['code'] ?? 0;
        buffer.writeString('\r\n[Shell exited with status $code]\r\n');
        state = ShellPaneState.exited;
        _release(terminate: false);
        notifyListeners();
        return;
      } else if (type == 'error') {
        errorMessage = event['message'] as String? ?? 'Shell disconnected';
        state = ShellPaneState.disconnected;
        _release(terminate: false);
        notifyListeners();
        return;
      }
    }
    if (reply['state'] == 'disconnected') {
      state = ShellPaneState.disconnected;
      _release(terminate: false);
    }
    notifyListeners();
  }

  List<int> _reconcileEcho(List<int> incoming) {
    if (_pendingEcho.isEmpty) return incoming;
    var index = 0;
    while (index < incoming.length &&
        _pendingEcho.isNotEmpty &&
        incoming[index] == _pendingEcho.first) {
      _pendingEcho.removeAt(0);
      index++;
    }
    if (index < incoming.length && _pendingEcho.isNotEmpty) {
      _pendingEcho.clear();
    }
    return incoming.sublist(index);
  }

  Future<void> sendInput(String data, {bool predictive = false}) async {
    final active = handle;
    if (_closed || active == null || state != ShellPaneState.attached) return;
    if (android) {
      await backend.invoke('nativeTerminalInput', {
        ...nativeArguments,
        'data': data,
      });
      return;
    }
    final bytes = utf8.encode(data);
    if (predictive &&
        echoEnabled &&
        canonical &&
        bytes.every((b) => b >= 0x20 && b <= 0x7E)) {
      _pendingEcho.addAll(bytes);
      buffer.writeBytes(bytes);
      notifyListeners();
    }
    try {
      await backend.invoke('shellInput', {
        'session': active,
        'data_b64': base64Encode(bytes),
      });
    } catch (error) {
      if (_closed) return;
      state = ShellPaneState.disconnected;
      errorMessage = error is PlatformException
          ? (error.message ?? error.code)
          : error.toString();
      notifyListeners();
    }
  }

  Future<void> resize(int cols, int rows) async {
    if (android) return; // Native viewport metrics drive its PTY dimensions.
    final c = cols.clamp(2, 500);
    final r = rows.clamp(2, 300);
    if (c == buffer.cols && r == buffer.rows) return;
    buffer.resize(c, r);
    notifyListeners();
    final active = handle;
    if (_closed || active == null || state != ShellPaneState.attached) return;
    try {
      await backend.invoke('shellResize', {
        'session': active,
        'cols': c,
        'rows': r,
      });
    } catch (_) {}
  }

  void _release({required bool terminate}) {
    _pollTimer?.cancel();
    _pollTimer = null;
    final old = handle;
    handle = null;
    if (old != null) {
      if (android) {
        // Activity/sheet disposal detaches Flutter while the native stream and
        // IME-independent terminal buffer continue running in the UI process.
        if (terminate) {
          unawaited(
            backend
                .invoke('nativeTerminalClose', {
                  ...nativeArguments,
                  'terminate': true,
                })
                .catchError((_) => <String, dynamic>{}),
          );
        }
        return;
      }
      unawaited(
        backend
            .invoke('shellClose', {'session': old, 'terminate': terminate})
            .catchError((_) => <String, dynamic>{}),
      );
    }
  }

  void close({required bool terminate}) {
    if (_closed) return;
    _closed = true;
    _terminateOnClose = terminate;
    _release(terminate: terminate);
    focusNode.dispose();
  }
}

class DesktopShellTab {
  DesktopShellTab({required this.id, required this.initialPane})
    : panes = [initialPane],
      activePane = initialPane;

  final String id;
  final DesktopShellPane initialPane;
  final List<DesktopShellPane> panes;
  DesktopShellPane activePane;
  Axis splitAxis = Axis.horizontal;

  String get label =>
      activePane.title ??
      (panes.length > 1
          ? '${initialPane.session} (${panes.length})'
          : initialPane.session);
}

class HostShellWorkspace extends ChangeNotifier {
  HostShellWorkspace({
    required this.backend,
    required this.host,
    required this.hostName,
    required this.service,
    required this.incarnation,
    this.android = false,
  });

  final WispnetBackend backend;
  final String host;
  String hostName;
  final String service;
  String incarnation;
  final bool android;
  final List<DesktopShellTab> tabs = [];
  late DesktopShellTab selectedTab;
  bool minimized = false;
  int _counter = 1;

  DesktopShellPane get activePane => selectedTab.activePane;

  void openSession(String sessionName) {
    minimized = false;
    for (final tab in tabs) {
      for (final pane in tab.panes) {
        if (pane.session == sessionName) {
          selectedTab = tab;
          tab.activePane = pane;
          pane.addListener(_forwardChange);
          notifyListeners();
          return;
        }
      }
    }
    final pane = DesktopShellPane(
      backend: backend,
      host: host,
      hostName: hostName,
      service: service,
      incarnation: incarnation,
      session: sessionName,
      android: android,
    )..addListener(_forwardChange);
    final tab = DesktopShellTab(id: sessionName, initialPane: pane);
    tabs.add(tab);
    selectedTab = tab;
    unawaited(pane.connect());
    notifyListeners();
  }

  void addTab() {
    if (tabs.length >= 16) return;
    _counter++;
    openSession('tab-$_counter');
  }

  void splitActivePane(Axis axis) {
    if (android) return;
    final tab = selectedTab;
    if (tab.panes.length >= 4) return;
    tab.splitAxis = axis;
    _counter++;
    final pane = DesktopShellPane(
      backend: backend,
      host: host,
      hostName: hostName,
      service: service,
      incarnation: incarnation,
      session: 'pane-$_counter',
      android: android,
    )..addListener(_forwardChange);
    tab.panes.add(pane);
    tab.activePane = pane;
    unawaited(pane.connect());
    notifyListeners();
  }

  /// Closes a single pane; returns true if the entire host workspace is now empty.
  bool closePane(
    DesktopShellTab tab,
    DesktopShellPane pane, {
    bool terminate = true,
  }) {
    pane.removeListener(_forwardChange);
    pane.close(terminate: terminate);
    tab.panes.remove(pane);
    if (tab.panes.isEmpty) {
      return closeTab(tab, terminate: false);
    }
    if (tab.activePane == pane) {
      tab.activePane = tab.panes.first;
    }
    notifyListeners();
    return false;
  }

  /// Closes a tab and all its panes; returns true if no tabs remain for this host.
  bool closeTab(DesktopShellTab tab, {bool terminate = true}) {
    for (final pane in tab.panes) {
      pane.removeListener(_forwardChange);
      pane.close(terminate: terminate);
    }
    tab.panes.clear();
    tabs.remove(tab);
    if (tabs.isEmpty) {
      minimized = false;
      notifyListeners();
      return true;
    }
    if (selectedTab == tab) {
      selectedTab = tabs.first;
    }
    notifyListeners();
    return false;
  }

  void closeAll({required bool terminate, bool notify = true}) {
    for (final tab in tabs) {
      for (final pane in tab.panes) {
        pane.removeListener(_forwardChange);
        pane.close(terminate: terminate);
      }
    }
    tabs.clear();
    minimized = false;
    if (notify) notifyListeners();
  }

  Map<String, dynamic> snapshotJson() {
    final pane = activePane;
    final suffix = selectedTab.label;
    final countBadge = tabs.length > 1 ? ' (${tabs.length})' : '';
    final stateName = switch (pane.state) {
      ShellPaneState.attached => 'attached',
      ShellPaneState.connecting => 'connecting',
      ShellPaneState.disconnected => 'disconnected',
      ShellPaneState.exited => 'exited',
      ShellPaneState.failed => 'failed',
    };
    return {
      'host': host,
      'host_name': hostName,
      'service': service,
      'incarnation': incarnation,
      'session': pane.session,
      'title': '$hostName · $suffix$countBadge',
      'state': stateName,
      'tab_count': tabs.length,
      'minimized': minimized,
    };
  }

  Map<String, dynamic> layoutJson() => {
    'host': host,
    'host_name': hostName,
    'service': service,
    'incarnation': incarnation,
    'minimized': minimized,
    'selected': selectedTab.id,
    'tabs': [
      for (final tab in tabs)
        {
          'id': tab.id,
          'axis': tab.splitAxis.name,
          'active': tab.activePane.session,
          'panes': tab.panes.map((pane) => pane.session).toList(),
        },
    ],
  };

  void restoreTabs(List<dynamic> saved, String? selected) {
    final used = <String>{};
    String? selectedSession;
    // Legacy phone layouts may contain up to 16 tabs with four panes each.
    for (final entry in saved.whereType<Map>().take(android ? 64 : 16)) {
      if (android && tabs.length >= 64) break;
      final sessions = (entry['panes'] as List? ?? [])
          .whereType<String>()
          .where((name) => RegExp(r'^[a-z0-9._-]{1,64}$').hasMatch(name))
          .where(used.add)
          .take(android ? math.min(4, 64 - tabs.length) : 4)
          .toList();
      if (sessions.isEmpty) continue;
      final panes = [
        for (final session in sessions)
          DesktopShellPane(
            backend: backend,
            host: host,
            hostName: hostName,
            service: service,
            incarnation: incarnation,
            session: session,
            android: android,
          )..addListener(_forwardChange),
      ];
      if (android) {
        // Preserve sessions from older split layouts as separate phone tabs.
        for (final pane in panes) {
          tabs.add(DesktopShellTab(id: pane.session, initialPane: pane));
        }
        if (selected != null &&
            (entry['id'] as String? ?? sessions.first) == selected) {
          selectedSession = sessions.contains(entry['active'])
              ? entry['active'] as String
              : sessions.first;
        }
      } else {
        final tab = DesktopShellTab(
          id: entry['id'] as String? ?? sessions.first,
          initialPane: panes.first,
        );
        tab.panes.addAll(panes.skip(1));
        tab.splitAxis = entry['axis'] == 'vertical'
            ? Axis.vertical
            : Axis.horizontal;
        tab.activePane = panes.firstWhere(
          (pane) => pane.session == entry['active'],
          orElse: () => panes.first,
        );
        tabs.add(tab);
      }
      for (final session in sessions) {
        final suffix = int.tryParse(session.split('-').last) ?? 1;
        _counter = math.max(_counter, suffix);
      }
    }
    if (tabs.isEmpty) return;
    selectedTab = tabs.firstWhere(
      (tab) => tab.id == (android ? selectedSession ?? selected : selected),
      orElse: () => tabs.first,
    );
    for (final tab in tabs) {
      for (final pane in tab.panes) {
        unawaited(pane.connect());
      }
    }
  }

  void _forwardChange() => notifyListeners();
}

class DesktopShellManager extends ChangeNotifier {
  DesktopShellManager(this.backend);
  final WispnetBackend backend;
  bool android = false;
  final Map<String, HostShellWorkspace> _workspaces = {};
  DesktopInputMode _inputMode = DesktopInputMode.predictive;
  bool _restored = false;
  bool _restoring = false;
  String? _savedLayout;
  DesktopInputMode get inputMode => _inputMode;
  set inputMode(DesktopInputMode value) {
    _inputMode = value;
    _changed();
  }

  Map<String, dynamic> layoutJson() => {
    'mode': inputMode.name,
    'workspaces': [
      for (final workspace in _workspaces.values)
        if (workspace.tabs.isNotEmpty) workspace.layoutJson(),
    ],
  };

  void persistLayout() {
    if (!android || _restoring) return;
    final layout = layoutJson();
    final encoded = jsonEncode(layout);
    if (encoded == _savedLayout) return;
    _savedLayout = encoded;
    unawaited(
      backend
          .invoke('saveTerminalLayout', {'layout': layout})
          .catchError((_) => <String, dynamic>{}),
    );
  }

  void _changed() {
    persistLayout();
    notifyListeners();
  }

  void restoreLayout(Map<String, dynamic> layout) {
    if (!android || _restored) return;
    _restored = true;
    _restoring = true;
    _inputMode = DesktopInputMode.values.firstWhere(
      (mode) => mode.name == layout['mode'],
      orElse: () => DesktopInputMode.predictive,
    );
    for (final raw
        in (layout['workspaces'] as List? ?? []).whereType<Map>().take(32)) {
      final host = raw['host'];
      final service = raw['service'];
      final incarnation = raw['incarnation'];
      if (host is! String ||
          !RegExp(r'^[0-9a-f]{64}$').hasMatch(host) ||
          service is! String ||
          !RegExp(r'^[a-z0-9][a-z0-9._-]{0,63}$').hasMatch(service) ||
          incarnation is! String ||
          !RegExp(r'^[0-9a-f]{64}$').hasMatch(incarnation) ||
          _workspaces.containsKey(host)) {
        continue;
      }
      final workspace = HostShellWorkspace(
        backend: backend,
        host: host,
        hostName: raw['host_name'] as String? ?? 'Computer',
        service: service,
        incarnation: incarnation,
        android: true,
      );
      workspace.restoreTabs(
        raw['tabs'] as List? ?? [],
        raw['selected'] as String?,
      );
      if (workspace.tabs.isEmpty) continue;
      // Restored pages begin minimized; the native streams retain their buffers.
      workspace.minimized = true;
      workspace.addListener(_changed);
      _workspaces[host] = workspace;
    }
    _restoring = false;
    _changed();
  }

  HostShellWorkspace openHost({
    required String host,
    required String hostName,
    required String service,
    required String incarnation,
    String session = 'main',
  }) {
    final workspace = _workspaces.putIfAbsent(
      host,
      () => HostShellWorkspace(
        backend: backend,
        host: host,
        hostName: hostName,
        service: service,
        incarnation: incarnation,
        android: android,
      )..addListener(_changed),
    );
    workspace.hostName = hostName;
    workspace.incarnation = incarnation;
    if (workspace.tabs.isNotEmpty && session == 'main') {
      workspace.minimized = false;
      for (final tab in workspace.tabs) {
        for (final pane in tab.panes) {
          if (pane.session == session) {
            workspace.selectedTab = tab;
            tab.activePane = pane;
            break;
          }
        }
      }
      // ignore: invalid_use_of_protected_member, invalid_use_of_visible_for_testing_member
      workspace.notifyListeners();
      return workspace;
    }
    workspace.openSession(session);
    return workspace;
  }

  List<Map<String, dynamic>> get minimizedSnapshots => _workspaces.values
      .where((w) => w.tabs.isNotEmpty && w.minimized)
      .map((w) => w.snapshotJson())
      .toList();

  void closeHost(String host, {bool terminate = true}) {
    final workspace = _workspaces.remove(host);
    if (workspace != null) {
      workspace.removeListener(_changed);
      workspace.closeAll(terminate: terminate);
      _changed();
    }
  }

  void pruneRemovedHosts(Set<String> validHosts) {
    final stale = _workspaces.keys
        .where((host) => !validHosts.contains(host))
        .toList();
    for (final host in stale) {
      closeHost(host, terminate: true);
    }
  }

  void closeAll({bool terminate = true, bool notify = true}) {
    for (final workspace in _workspaces.values) {
      workspace.removeListener(_changed);
      workspace.closeAll(terminate: terminate, notify: notify);
    }
    _workspaces.clear();
    if (notify) _changed();
  }
}

class DesktopTerminalSheet extends StatefulWidget {
  const DesktopTerminalSheet({
    super.key,
    required this.manager,
    required this.workspace,
    required this.onOpenExternal,
  });

  final DesktopShellManager manager;
  final HostShellWorkspace workspace;
  final Future<void> Function(DesktopShellPane pane) onOpenExternal;

  @override
  State<DesktopTerminalSheet> createState() => _DesktopTerminalSheetState();
}

class _DesktopTerminalSheetState extends State<DesktopTerminalSheet> {
  bool _modeMenu = false;
  bool _closedExplicitly = false;
  bool _popped = false;
  final TextEditingController _lineController = TextEditingController();
  final FocusNode _lineFocus = FocusNode();

  HostShellWorkspace get workspace => widget.workspace;

  @override
  void initState() {
    super.initState();
    workspace.addListener(_onWorkspaceChanged);
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted && workspace.tabs.isNotEmpty) {
        workspace.activePane.focusNode.requestFocus();
      }
    });
  }

  @override
  void dispose() {
    workspace.removeListener(_onWorkspaceChanged);
    _lineController.dispose();
    _lineFocus.dispose();
    if (!_closedExplicitly && workspace.tabs.isNotEmpty) {
      workspace.minimized = true;
      widget.manager.persistLayout();
    }
    super.dispose();
  }

  void _popSheet() {
    if (_popped || !mounted) return;
    _popped = true;
    final navigator = Navigator.of(context);
    if (navigator.canPop()) {
      navigator.pop();
    }
  }

  void _onWorkspaceChanged() {
    if (!mounted || _popped) return;
    if (workspace.tabs.isEmpty) {
      _closedExplicitly = true;
      widget.manager.closeHost(workspace.host, terminate: false);
      _popSheet();
      return;
    }
    setState(() {});
  }

  void _minimize() {
    workspace.minimized = true;
    widget.manager.persistLayout();
    _closedExplicitly = true;
    _popSheet();
  }

  Future<void> _confirmClose() async {
    final confirmed = await showWispSheet<bool>(
      context,
      title: 'Close terminal?',
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Text(
            'Close all terminal sessions on ${workspace.hostName}? Running commands in those tabs will be terminated.',
          ),
          const SizedBox(height: 16),
          FilledButton(
            key: const ValueKey('desktop-shell-confirm-close'),
            onPressed: () => Navigator.pop(context, true),
            child: const Text('Close sessions'),
          ),
          TextButton(
            onPressed: () => Navigator.pop(context, false),
            child: const Text('Cancel'),
          ),
        ],
      ),
    );
    if (confirmed == true && mounted) {
      widget.manager.closeHost(workspace.host, terminate: true);
    }
  }

  @override
  Widget build(BuildContext context) {
    if (workspace.tabs.isEmpty) return const SizedBox.shrink();
    final pane = workspace.activePane;
    final effectiveMode = widget.manager.inputMode.resolve(
      latencyMs: pane.latencyMs,
      echo: pane.echoEnabled,
      canonical: pane.canonical,
    );
    final statusText = switch (pane.state) {
      ShellPaneState.attached =>
        pane.latencyMs != null ? '${pane.latencyMs} ms' : '',
      ShellPaneState.connecting => 'Connecting…',
      ShellPaneState.disconnected => 'Disconnected',
      ShellPaneState.failed => pane.errorMessage ?? 'Connection failed',
      ShellPaneState.exited => 'Shell exited',
    };
    final dotColor = switch (pane.state) {
      ShellPaneState.attached => WispColors.green,
      ShellPaneState.connecting => WispColors.amber,
      ShellPaneState.disconnected ||
      ShellPaneState.exited ||
      ShellPaneState.failed => WispColors.error,
    };
    final terminalIdentity = Row(
      children: [
        WispStatusDot(color: dotColor),
        const SizedBox(width: 8),
        Flexible(
          child: Text(
            workspace.hostName,
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: const TextStyle(fontSize: 15, fontWeight: FontWeight.bold),
          ),
        ),
        Expanded(
          child: Text(
            statusText.isNotEmpty ? ' · $statusText' : '',
            maxLines: 1,
            overflow: TextOverflow.ellipsis,
            style: const TextStyle(fontSize: 12, color: WispColors.muted),
          ),
        ),
        if (pane.state == ShellPaneState.disconnected ||
            pane.state == ShellPaneState.exited ||
            pane.state == ShellPaneState.failed) ...[
          TextButton(
            onPressed: pane.connect,
            child: Text(
              pane.state == ShellPaneState.exited ? 'Restart' : 'Reconnect',
            ),
          ),
          const SizedBox(width: 4),
        ],
      ],
    );
    final terminalControls = Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Semantics(
          button: true,
          label: 'Input mode',
          child: InkWell(
            key: const ValueKey('desktop-shell-mode-button'),
            borderRadius: BorderRadius.circular(14),
            onTap: () => setState(() => _modeMenu = !_modeMenu),
            child: Container(
              height: 28,
              padding: const EdgeInsets.symmetric(horizontal: 10),
              alignment: Alignment.center,
              decoration: BoxDecoration(
                color: _modeMenu
                    ? const Color(0xFF1F3666)
                    : const Color(0xFF142038),
                borderRadius: BorderRadius.circular(14),
                border: Border.all(
                  color: _modeMenu
                      ? WispColors.borderActive
                      : WispColors.border,
                ),
              ),
              child: Text(
                widget.manager.inputMode == DesktopInputMode.auto
                    ? 'Auto · ${effectiveMode.label}'
                    : widget.manager.inputMode.label,
                style: const TextStyle(
                  fontSize: 12,
                  fontWeight: FontWeight.w500,
                ),
              ),
            ),
          ),
        ),
        if (!widget.manager.android) ...[
          const SizedBox(width: 6),
          _HeaderCircleButton(
            key: const ValueKey('desktop-shell-split-vertical'),
            tooltip: 'Split side by side',
            icon: Icons.vertical_split_outlined,
            onTap: () => workspace.splitActivePane(Axis.horizontal),
          ),
          const SizedBox(width: 6),
          _HeaderCircleButton(
            key: const ValueKey('desktop-shell-split-horizontal'),
            tooltip: 'Split top and bottom',
            icon: Icons.horizontal_split_outlined,
            onTap: () => workspace.splitActivePane(Axis.vertical),
          ),
          const SizedBox(width: 6),
          _HeaderCircleButton(
            key: const ValueKey('desktop-shell-open-external'),
            tooltip: 'Open in system terminal',
            icon: Icons.open_in_new,
            onTap: () => unawaited(widget.onOpenExternal(pane)),
          ),
        ],
        const SizedBox(width: 6),
        _HeaderCircleButton(
          key: const ValueKey('desktop-shell-close'),
          tooltip: 'Minimize',
          icon: Icons.remove,
          onTap: _minimize,
        ),
        if (!widget.manager.android) ...[
          const SizedBox(width: 6),
          _HeaderCircleButton(
            key: const ValueKey('desktop-shell-terminate'),
            tooltip: 'Close sessions',
            icon: Icons.close,
            onTap: () => unawaited(_confirmClose()),
          ),
        ],
      ],
    );
    return LayoutBuilder(
      builder: (context, constraints) => SizedBox(
        height: constraints.maxHeight,
        child: Stack(
          children: [
            Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              children: [
                LayoutBuilder(
                  builder: (context, constraints) =>
                      !widget.manager.android && constraints.maxWidth < 640
                      ? Column(
                          crossAxisAlignment: CrossAxisAlignment.stretch,
                          children: [
                            terminalIdentity,
                            const SizedBox(height: 6),
                            SingleChildScrollView(
                              scrollDirection: Axis.horizontal,
                              child: terminalControls,
                            ),
                          ],
                        )
                      : Row(
                          children: [
                            Expanded(child: terminalIdentity),
                            const SizedBox(width: 6),
                            terminalControls,
                          ],
                        ),
                ),
                const SizedBox(height: 6),
                // Second row: Tabs for this host + "+" button
                SingleChildScrollView(
                  scrollDirection: Axis.horizontal,
                  child: Row(
                    children: [
                      for (final tab in workspace.tabs)
                        Padding(
                          padding: const EdgeInsets.only(right: 6),
                          child: InkWell(
                            key: ValueKey('desktop-shell-tab-${tab.id}'),
                            borderRadius: BorderRadius.circular(8),
                            onTap: () {
                              setState(() => workspace.selectedTab = tab);
                              tab.activePane.focusNode.requestFocus();
                            },
                            child: Container(
                              height: 28,
                              padding: EdgeInsets.only(
                                left: 10,
                                right: widget.manager.android ? 10 : 6,
                              ),
                              decoration: BoxDecoration(
                                color: tab == workspace.selectedTab
                                    ? const Color(0xFF1F3666)
                                    : WispColors.panel,
                                borderRadius: BorderRadius.circular(8),
                                border: Border.all(
                                  color: tab == workspace.selectedTab
                                      ? WispColors.borderActive
                                      : WispColors.border,
                                ),
                              ),
                              child: Row(
                                mainAxisSize: MainAxisSize.min,
                                children: [
                                  Text(
                                    tab.label,
                                    style: TextStyle(
                                      fontSize: 12,
                                      fontWeight: tab == workspace.selectedTab
                                          ? FontWeight.w600
                                          : FontWeight.normal,
                                      color: tab == workspace.selectedTab
                                          ? WispColors.text
                                          : WispColors.muted,
                                    ),
                                  ),
                                  if (!widget.manager.android) ...[
                                    const SizedBox(width: 6),
                                    InkWell(
                                      key: ValueKey(
                                        'desktop-shell-close-tab-${tab.id}',
                                      ),
                                      borderRadius: BorderRadius.circular(9),
                                      onTap: () => workspace.closeTab(
                                        tab,
                                        terminate: true,
                                      ),
                                      child: Padding(
                                        padding: const EdgeInsets.all(2),
                                        child: Icon(
                                          Icons.close,
                                          size: 13,
                                          color: tab == workspace.selectedTab
                                              ? WispColors.text
                                              : WispColors.muted,
                                        ),
                                      ),
                                    ),
                                  ],
                                ],
                              ),
                            ),
                          ),
                        ),
                      if (workspace.tabs.length < 16)
                        InkWell(
                          key: const ValueKey('desktop-shell-new-tab'),
                          borderRadius: BorderRadius.circular(8),
                          onTap: workspace.addTab,
                          child: Container(
                            width: 28,
                            height: 28,
                            alignment: Alignment.center,
                            decoration: BoxDecoration(
                              color: WispColors.panel,
                              borderRadius: BorderRadius.circular(8),
                              border: Border.all(color: WispColors.border),
                            ),
                            child: const Text(
                              '+',
                              style: TextStyle(
                                fontSize: 15,
                                fontWeight: FontWeight.bold,
                                color: WispColors.cyan,
                              ),
                            ),
                          ),
                        ),
                    ],
                  ),
                ),
                const SizedBox(height: 6),
                // Split-pane terminal viewport
                Expanded(
                  child: _SplitViewport(
                    workspace: workspace,
                    tab: workspace.selectedTab,
                    effectiveMode: effectiveMode,
                    onKeyboardRequest: _lineFocus.requestFocus,
                  ),
                ),
                if (effectiveMode == DesktopInputMode.line) ...[
                  const SizedBox(height: 6),
                  Row(
                    children: [
                      Expanded(
                        child: TextField(
                          key: const ValueKey('desktop-shell-line-input'),
                          controller: _lineController,
                          focusNode: _lineFocus,
                          style: const TextStyle(
                            fontFamily: 'monospace',
                            fontSize: 14,
                          ),
                          decoration: const InputDecoration(
                            hintText: 'Enter command…',
                            isDense: true,
                          ),
                          onSubmitted: (value) {
                            unawaited(pane.sendInput('$value\r'));
                            _lineController.clear();
                          },
                        ),
                      ),
                      const SizedBox(width: 8),
                      FilledButton(
                        onPressed: () {
                          unawaited(
                            pane.sendInput('${_lineController.text}\r'),
                          );
                          _lineController.clear();
                        },
                        child: const Text('Send'),
                      ),
                    ],
                  ),
                ],
                const SizedBox(height: 4),
                // Shortcut keys bar
                SingleChildScrollView(
                  scrollDirection: Axis.horizontal,
                  child: Row(
                    mainAxisAlignment: MainAxisAlignment.spaceEvenly,
                    children: [
                      for (final entry in const [
                        ('Ctrl-C', '\u0003'),
                        ('Esc', '\u001b'),
                        ('Tab', '\t'),
                        ('↑', '\u001b[A'),
                        ('↓', '\u001b[B'),
                        ('←', '\u001b[D'),
                        ('→', '\u001b[C'),
                      ])
                        TextButton(
                          onPressed: pane.state == ShellPaneState.attached
                              ? () => unawaited(pane.sendInput(entry.$2))
                              : null,
                          child: Text(entry.$1),
                        ),
                    ],
                  ),
                ),
              ],
            ),
            if (_modeMenu)
              Positioned.fill(
                child: GestureDetector(
                  behavior: HitTestBehavior.opaque,
                  onTap: () => setState(() => _modeMenu = false),
                  child: Align(
                    alignment: Alignment.topRight,
                    child: Padding(
                      padding: const EdgeInsets.only(top: 36),
                      child: ConstrainedBox(
                        constraints: const BoxConstraints(maxWidth: 320),
                        child: WispPanel(
                          key: const ValueKey('desktop-shell-mode-popover'),
                          color: WispColors.panel,
                          radius: 14,
                          padding: const EdgeInsets.all(10),
                          child: SingleChildScrollView(
                            child: Column(
                              mainAxisSize: MainAxisSize.min,
                              crossAxisAlignment: CrossAxisAlignment.stretch,
                              children: [
                                const Padding(
                                  padding: EdgeInsets.symmetric(
                                    horizontal: 4,
                                    vertical: 2,
                                  ),
                                  child: Text(
                                    'Input mode',
                                    style: TextStyle(
                                      fontSize: 11,
                                      fontWeight: FontWeight.w600,
                                      color: WispColors.muted,
                                    ),
                                  ),
                                ),
                                for (final mode in DesktopInputMode.values) ...[
                                  const SizedBox(height: 6),
                                  InkWell(
                                    key: ValueKey(
                                      'desktop-shell-mode-${mode.name}',
                                    ),
                                    borderRadius: BorderRadius.circular(10),
                                    onTap: () => setState(() {
                                      widget.manager.inputMode = mode;
                                      _modeMenu = false;
                                    }),
                                    child: Container(
                                      padding: const EdgeInsets.symmetric(
                                        horizontal: 10,
                                        vertical: 8,
                                      ),
                                      decoration: BoxDecoration(
                                        color: mode == widget.manager.inputMode
                                            ? const Color(0xFF1F3666)
                                            : const Color(0xFF142038),
                                        borderRadius: BorderRadius.circular(10),
                                        border: Border.all(
                                          color:
                                              mode == widget.manager.inputMode
                                              ? WispColors.borderActive
                                              : WispColors.border,
                                        ),
                                      ),
                                      child: Column(
                                        crossAxisAlignment:
                                            CrossAxisAlignment.start,
                                        children: [
                                          Row(
                                            children: [
                                              Expanded(
                                                child: Text(
                                                  mode == DesktopInputMode.auto
                                                      ? 'Auto (currently ${effectiveMode.label})'
                                                      : mode.label,
                                                  style: const TextStyle(
                                                    fontSize: 13,
                                                    fontWeight: FontWeight.w600,
                                                  ),
                                                ),
                                              ),
                                              if (mode ==
                                                  widget.manager.inputMode)
                                                const Text(
                                                  'Active',
                                                  style: TextStyle(
                                                    fontSize: 11,
                                                    fontWeight: FontWeight.bold,
                                                    color: WispColors.cyan,
                                                  ),
                                                ),
                                            ],
                                          ),
                                          const SizedBox(height: 2),
                                          Text(
                                            mode.summary,
                                            style: const TextStyle(
                                              fontSize: 11,
                                              color: WispColors.muted,
                                            ),
                                          ),
                                        ],
                                      ),
                                    ),
                                  ),
                                ],
                              ],
                            ),
                          ),
                        ),
                      ),
                    ),
                  ),
                ),
              ),
          ],
        ),
      ),
    );
  }
}

class _HeaderCircleButton extends StatelessWidget {
  const _HeaderCircleButton({
    super.key,
    required this.tooltip,
    required this.icon,
    required this.onTap,
  });

  final String tooltip;
  final IconData icon;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    return Tooltip(
      message: tooltip,
      child: InkWell(
        borderRadius: BorderRadius.circular(14),
        onTap: onTap,
        child: Container(
          width: 28,
          height: 28,
          alignment: Alignment.center,
          decoration: BoxDecoration(
            color: const Color(0xFF142038),
            shape: BoxShape.circle,
            border: Border.all(color: WispColors.border),
          ),
          child: Icon(icon, size: 15, color: WispColors.text),
        ),
      ),
    );
  }
}

class _SplitViewport extends StatelessWidget {
  const _SplitViewport({
    required this.workspace,
    required this.tab,
    required this.effectiveMode,
    required this.onKeyboardRequest,
  });

  final HostShellWorkspace workspace;
  final DesktopShellTab tab;
  final DesktopInputMode effectiveMode;
  final VoidCallback onKeyboardRequest;

  @override
  Widget build(BuildContext context) {
    final panes = tab.panes;
    final multiple = panes.length > 1;
    final children = <Widget>[
      for (var i = 0; i < panes.length; i++) ...[
        if (i > 0)
          SizedBox(
            width: tab.splitAxis == Axis.horizontal ? 6 : 0,
            height: tab.splitAxis == Axis.vertical ? 6 : 0,
          ),
        Expanded(
          child: _TerminalPaneView(
            key: ValueKey('desktop-shell-pane-${panes[i].session}'),
            pane: panes[i],
            focused: panes[i] == tab.activePane,
            showPaneControls: multiple && !workspace.android,
            effectiveMode: effectiveMode,
            onKeyboardRequest: onKeyboardRequest,
            onFocus: () {
              tab.activePane = panes[i];
              panes[i].focusNode.requestFocus();
              // ignore: invalid_use_of_protected_member, invalid_use_of_visible_for_testing_member
              workspace.notifyListeners();
            },
            onClose: () => workspace.closePane(tab, panes[i], terminate: true),
          ),
        ),
      ],
    ];
    return tab.splitAxis == Axis.horizontal
        ? Row(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: children,
          )
        : Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: children,
          );
  }
}

class _TerminalPaneView extends StatelessWidget {
  const _TerminalPaneView({
    super.key,
    required this.pane,
    required this.focused,
    required this.showPaneControls,
    required this.effectiveMode,
    required this.onKeyboardRequest,
    required this.onFocus,
    required this.onClose,
  });

  final DesktopShellPane pane;
  final bool focused;
  final bool showPaneControls;
  final DesktopInputMode effectiveMode;
  final VoidCallback onKeyboardRequest;
  final VoidCallback onFocus;
  final VoidCallback onClose;

  KeyEventResult _handleKeyEvent(FocusNode node, KeyEvent event) {
    if (pane.android) return KeyEventResult.ignored;
    if (event is! KeyDownEvent && event is! KeyRepeatEvent) {
      return KeyEventResult.ignored;
    }
    if (effectiveMode == DesktopInputMode.line) {
      return KeyEventResult.ignored;
    }
    final key = event.logicalKey;
    final ctrl = HardwareKeyboard.instance.isControlPressed;
    final meta = HardwareKeyboard.instance.isMetaPressed;
    final shift = HardwareKeyboard.instance.isShiftPressed;

    if ((ctrl && shift && key == LogicalKeyboardKey.keyV) ||
        (meta && key == LogicalKeyboardKey.keyV)) {
      unawaited(
        Clipboard.getData(Clipboard.kTextPlain).then((clip) {
          final text = clip?.text;
          if (text != null && text.isNotEmpty) {
            pane.sendInput(text);
          }
        }),
      );
      return KeyEventResult.handled;
    }
    if (ctrl) {
      final label = key.keyLabel.toLowerCase();
      if (label.length == 1) {
        final code = label.codeUnitAt(0);
        if (code >= 97 && code <= 122) {
          unawaited(pane.sendInput(String.fromCharCode(code - 96)));
          return KeyEventResult.handled;
        }
      }
    }
    final mapped = switch (key) {
      LogicalKeyboardKey.enter || LogicalKeyboardKey.numpadEnter => '\r',
      LogicalKeyboardKey.backspace => '\u007f',
      LogicalKeyboardKey.tab => '\t',
      LogicalKeyboardKey.escape => '\u001b',
      LogicalKeyboardKey.arrowUp => '\u001b[A',
      LogicalKeyboardKey.arrowDown => '\u001b[B',
      LogicalKeyboardKey.arrowRight => '\u001b[C',
      LogicalKeyboardKey.arrowLeft => '\u001b[D',
      LogicalKeyboardKey.home => '\u001b[H',
      LogicalKeyboardKey.end => '\u001b[F',
      LogicalKeyboardKey.delete => '\u001b[3~',
      LogicalKeyboardKey.pageUp => '\u001b[5~',
      LogicalKeyboardKey.pageDown => '\u001b[6~',
      _ => null,
    };
    if (mapped != null) {
      unawaited(pane.sendInput(mapped));
      return KeyEventResult.handled;
    }
    final ch = event.character;
    if (ch != null && ch.isNotEmpty && !ctrl && !meta) {
      unawaited(
        pane.sendInput(
          ch,
          predictive: effectiveMode == DesktopInputMode.predictive,
        ),
      );
      return KeyEventResult.handled;
    }
    return KeyEventResult.ignored;
  }

  @override
  Widget build(BuildContext context) {
    return GestureDetector(
      behavior: HitTestBehavior.opaque,
      onTap: onFocus,
      child: Focus(
        focusNode: pane.focusNode,
        autofocus: focused,
        onKeyEvent: _handleKeyEvent,
        child: Container(
          decoration: BoxDecoration(
            color: Colors.black,
            borderRadius: BorderRadius.circular(14),
            border: Border.all(
              color: showPaneControls && focused
                  ? WispColors.borderActive
                  : const Color(0xFF1A2947),
            ),
          ),
          child: Stack(
            children: [
              Positioned.fill(
                child: Padding(
                  padding: const EdgeInsets.symmetric(
                    horizontal: 8,
                    vertical: 6,
                  ),
                  child: pane.android
                      ? _NativeTerminalViewport(
                          pane: pane,
                          focused: focused,
                          mode: effectiveMode,
                          onFocus: onFocus,
                          onKeyboardRequest: onKeyboardRequest,
                        )
                      : LayoutBuilder(
                          builder: (context, constraints) {
                            const charWidth = 8.4;
                            const lineHeight = 17.0;
                            final cols = math.max(
                              20,
                              (constraints.maxWidth / charWidth).floor(),
                            );
                            final rows = math.max(
                              5,
                              (constraints.maxHeight / lineHeight).floor(),
                            );
                            if (cols != pane.buffer.cols ||
                                rows != pane.buffer.rows) {
                              WidgetsBinding.instance.addPostFrameCallback((_) {
                                unawaited(pane.resize(cols, rows));
                              });
                            }
                            return CustomPaint(
                              painter: _TerminalCanvasPainter(
                                buffer: pane.buffer,
                                version: pane.buffer.version,
                                cursorRow: pane.buffer.cursorRow,
                                cursorCol: pane.buffer.cursorCol,
                                showCursor:
                                    focused &&
                                    pane.state == ShellPaneState.attached,
                              ),
                            );
                          },
                        ),
                ),
              ),
              if (showPaneControls)
                Positioned(
                  top: 6,
                  right: 6,
                  child: Row(
                    mainAxisSize: MainAxisSize.min,
                    children: [
                      Container(
                        padding: const EdgeInsets.symmetric(
                          horizontal: 6,
                          vertical: 2,
                        ),
                        decoration: BoxDecoration(
                          color: const Color(0xCC101A30),
                          borderRadius: BorderRadius.circular(6),
                          border: Border.all(color: WispColors.border),
                        ),
                        child: Text(
                          pane.title ?? pane.session,
                          style: const TextStyle(
                            fontSize: 10,
                            color: WispColors.muted,
                          ),
                        ),
                      ),
                      const SizedBox(width: 4),
                      InkWell(
                        key: ValueKey(
                          'desktop-shell-close-pane-${pane.session}',
                        ),
                        borderRadius: BorderRadius.circular(10),
                        onTap: onClose,
                        child: Container(
                          width: 20,
                          height: 20,
                          alignment: Alignment.center,
                          decoration: BoxDecoration(
                            color: const Color(0xCC142038),
                            shape: BoxShape.circle,
                            border: Border.all(color: WispColors.border),
                          ),
                          child: const Icon(
                            Icons.close,
                            size: 12,
                            color: WispColors.muted,
                          ),
                        ),
                      ),
                    ],
                  ),
                ),
            ],
          ),
        ),
      ),
    );
  }
}

/// Android's terminal renderer and composition-aware IME stay native while all
/// surrounding controls, tabs, panes and lifecycle decisions are shared.
class _NativeTerminalViewport extends StatefulWidget {
  const _NativeTerminalViewport({
    required this.pane,
    required this.focused,
    required this.mode,
    required this.onFocus,
    required this.onKeyboardRequest,
  });
  final DesktopShellPane pane;
  final bool focused;
  final DesktopInputMode mode;
  final VoidCallback onFocus;
  final VoidCallback onKeyboardRequest;

  @override
  State<_NativeTerminalViewport> createState() =>
      _NativeTerminalViewportState();
}

class _NativeTerminalViewportState extends State<_NativeTerminalViewport> {
  MethodChannel? channel;

  Map<String, dynamic> get configuration => {
    ...widget.pane.nativeArguments,
    'active': widget.focused,
    'mode': widget.mode.name,
  };

  void update() {
    unawaited(
      channel?.invokeMethod<void>('update', configuration).catchError((_) {}),
    );
  }

  @override
  void didUpdateWidget(_NativeTerminalViewport oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (oldWidget.focused != widget.focused || oldWidget.mode != widget.mode) {
      update();
    }
  }

  @override
  void dispose() {
    channel?.setMethodCallHandler(null);
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => ClipRRect(
    borderRadius: BorderRadius.circular(8),
    child: AndroidView(
      viewType: 'app.wispnet/terminal',
      creationParams: configuration,
      creationParamsCodec: const StandardMessageCodec(),
      gestureRecognizers: {
        Factory<OneSequenceGestureRecognizer>(EagerGestureRecognizer.new),
      },
      onPlatformViewCreated: (id) {
        channel = MethodChannel('app.wispnet/terminal/$id')
          ..setMethodCallHandler((call) async {
            if (call.method == 'focus' && mounted) {
              widget.onFocus();
            }
            if (call.method == 'keyboard' && mounted) {
              widget.onKeyboardRequest();
            }
          });
        update();
      },
    ),
  );
}

class _TerminalCanvasPainter extends CustomPainter {
  _TerminalCanvasPainter({
    required this.buffer,
    required this.version,
    required this.cursorRow,
    required this.cursorCol,
    required this.showCursor,
  });

  final TerminalEmulatorBuffer buffer;
  final int version;
  final int cursorRow;
  final int cursorCol;
  final bool showCursor;

  static final Map<(String, int, bool), TextPainter> _painterCache = {};

  static TextPainter _layoutRun(String text, Color fg, bool bold) {
    final key = (text, fg.toARGB32(), bold);
    final cached = _painterCache[key];
    if (cached != null) return cached;
    if (_painterCache.length >= 1024) {
      _painterCache.clear();
    }
    final tp = TextPainter(
      text: TextSpan(
        text: text,
        style: TextStyle(
          fontFamily: 'monospace',
          fontSize: 13.5,
          fontWeight: bold ? FontWeight.bold : FontWeight.normal,
          color: fg,
        ),
      ),
      textDirection: TextDirection.ltr,
    )..layout();
    _painterCache[key] = tp;
    return tp;
  }

  @override
  void paint(Canvas canvas, Size size) {
    const lineHeight = 17.0;
    const charWidth = 8.4;

    final startRow = buffer.activeStartLine;
    final allLines = buffer.lines;
    final bgPaint = Paint();
    final runBuffer = StringBuffer();

    for (var r = 0; r < buffer.rows; r++) {
      final lineIdx = startRow + r;
      if (lineIdx >= allLines.length) break;
      final row = allLines[lineIdx];
      final y = r * lineHeight;

      // 1. Coalesce and draw non-transparent background spans.
      var c = 0;
      while (c < row.length) {
        final bg = row[c].bg;
        if (bg == Colors.transparent) {
          c++;
          continue;
        }
        final startCol = c;
        c++;
        while (c < row.length && row[c].bg == bg) {
          c++;
        }
        bgPaint.color = bg;
        canvas.drawRect(
          Rect.fromLTWH(
            startCol * charWidth,
            y,
            (c - startCol) * charWidth,
            lineHeight,
          ),
          bgPaint,
        );
      }

      // 2. Coalesce and draw contiguous non-space text runs with identical style.
      c = 0;
      while (c < row.length) {
        final first = row[c];
        if (first.char == ' ') {
          c++;
          continue;
        }
        final startCol = c;
        final fg = first.fg;
        final bold = first.bold;
        runBuffer.clear();
        runBuffer.write(first.char);
        c++;
        while (c < row.length) {
          final next = row[c];
          if (next.char == ' ' || next.fg != fg || next.bold != bold) {
            break;
          }
          runBuffer.write(next.char);
          c++;
        }
        final tp = _layoutRun(runBuffer.toString(), fg, bold);
        tp.paint(canvas, Offset(startCol * charWidth, y));
      }
    }

    if (showCursor) {
      final cx = cursorCol * charWidth;
      final cy = cursorRow * lineHeight;
      canvas.drawRect(
        Rect.fromLTWH(cx, cy + 1, charWidth, lineHeight - 2),
        Paint()..color = WispColors.cyan.withValues(alpha: 0.7),
      );
    }
  }

  @override
  bool shouldRepaint(covariant _TerminalCanvasPainter oldDelegate) =>
      oldDelegate.version != version ||
      oldDelegate.showCursor != showCursor ||
      oldDelegate.cursorRow != cursorRow ||
      oldDelegate.cursorCol != cursorCol;
}
