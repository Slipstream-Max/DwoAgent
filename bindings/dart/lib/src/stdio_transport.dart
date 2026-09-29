part of '../dwo_rpc.dart';

/// Local management transport backed by `executable rpc` and an existing daemon.
/// Closing this transport only closes/kills its own bridge child process.
final class DwoStdioTransport implements DwoTransport {
  DwoStdioTransport._(this._process, this._timeout) {
    _stdout = _process.stdout
        .transform(utf8.decoder)
        .transform(const LineSplitter())
        .listen(_onLine, onError: _fail, onDone: () {
      _fail(StateError('Dwo rpc stdout closed$_diagnostics'));
    });
    _stderr = _process.stderr.listen((bytes) {
      _stderrTail += utf8.decode(bytes, allowMalformed: true);
      if (_stderrTail.length > 8192) {
        _stderrTail = _stderrTail.substring(_stderrTail.length - 8192);
      }
    }, onError: (Object error) {
      _fail(error);
    });
    _process.exitCode.then((code) {
      _fail(StateError('Dwo rpc exited with code $code$_diagnostics'));
    });
    _process.stdin.done.then<void>((_) {}, onError: (Object error) {
      _fail(error);
    });
  }

  final Process _process;
  final Duration _timeout;
  final Map<String, Completer<JsonObject>> _pending = {};
  late final StreamSubscription<String> _stdout;
  late final StreamSubscription<List<int>> _stderr;
  StreamController<JsonObject>? _events;
  bool _subscribed = false;
  bool _closed = false;
  Future<void>? _closing;
  String _stderrTail = '';

  String get _diagnostics =>
      _stderrTail.isEmpty ? '' : ': ${_stderrTail.trim()}';

  /// Starts exactly `[executable, 'rpc']`, then checks `dwo.capabilities`.
  /// The timeout applies to startup and each request. No daemon is started.
  static Future<DwoStdioTransport> connect({
    required String executable,
    Duration timeout = const Duration(seconds: 30),
  }) async {
    if (timeout <= Duration.zero) {
      throw ArgumentError.value(timeout, 'timeout', 'must be positive');
    }
    final clock = Stopwatch()..start();
    var abandoned = false;
    final starting = Process.start(executable, const ['rpc']);
    // Also dispose of a process whose OS launch completes after the deadline.
    unawaited(starting.then<void>((process) {
      if (abandoned) {
        process.kill();
        unawaited(process.stdout.drain<void>());
        unawaited(process.stderr.drain<void>());
        unawaited(process.stdin.close().catchError((Object _) {}));
      }
    }, onError: (Object _) {}));
    final process = await starting.timeout(timeout, onTimeout: () {
      abandoned = true;
      throw TimeoutException('Starting dwo rpc timed out', timeout);
    });
    final transport = DwoStdioTransport._(process, timeout);
    try {
      final remaining = timeout - clock.elapsed;
      if (remaining <= Duration.zero) {
        throw TimeoutException('Connecting dwo rpc timed out', timeout);
      }
      await DwoRpcClient(transport).capabilities().timeout(remaining);
      return transport;
    } catch (_) {
      await transport.close();
      rethrow;
    }
  }

  @override
  Future<JsonObject> request(JsonObject envelope) async {
    if (_closed) throw StateError('Dwo stdio transport is closed');
    final id = envelope['id'];
    if (id is! String || id.isEmpty) {
      throw ArgumentError('Dwo request id must be a non-empty string');
    }
    if (envelope['route'] != 'dwo' || envelope['jsonrpc'] != '2.0') {
      throw ArgumentError(
          'Dwo stdio requests require jsonrpc 2.0 and route dwo');
    }
    if (_pending.containsKey(id)) {
      throw StateError('Dwo request id is already pending: $id');
    }
    final line = jsonEncode(envelope);
    final pending = Completer<JsonObject>();
    _pending[id] = pending;
    try {
      _process.stdin.writeln(line);
      return await pending.future.timeout(_timeout);
    } finally {
      if (identical(_pending[id], pending)) _pending.remove(id);
    }
  }

  @override
  Future<DwoRawSubscription> subscribe(JsonObject envelope) async {
    if (_closed) throw StateError('Dwo stdio transport is closed');
    if (envelope['method'] != 'event.subscribe') {
      throw ArgumentError('Dwo stdio subscriptions require event.subscribe');
    }
    // Event frames have no subscription id. Enforce a single stream to avoid
    // ambiguous delivery, duplicates, or events leaking between subscriptions.
    if (_subscribed) {
      throw StateError('Dwo stdio transport supports one event subscription');
    }
    _subscribed = true;
    final events = StreamController<JsonObject>();
    _events = events;
    try {
      final response = await request(envelope);
      if (response.containsKey('error')) {
        _events = null;
        unawaited(events.close());
      }
      return DwoRawSubscription(response, events.stream);
    } catch (_) {
      _events = null;
      unawaited(events.close());
      // An unacknowledged subscription may still be alive at the daemon.
      await close();
      rethrow;
    }
  }

  void _onLine(String line) {
    if (_closed || line.trim().isEmpty) return;
    try {
      final decoded = jsonDecode(line);
      if (decoded is! Map<String, dynamic>) {
        throw const FormatException('Dwo frame must be a JSON object');
      }
      final frame = Map<String, Object?>.from(decoded);
      if (frame['jsonrpc'] != '2.0') {
        throw const FormatException('Invalid Dwo RPC version');
      }
      if (frame.containsKey('id')) {
        final id = frame['id'];
        if (id is! String ||
            frame.containsKey('result') == frame.containsKey('error') ||
            (frame.containsKey('error') && frame['error'] is! Map)) {
          throw const FormatException('Invalid Dwo response envelope');
        }
        // Remove before completion: duplicate and late responses are harmless.
        _pending.remove(id)?.complete(frame);
      } else {
        DwoEvent.fromEnvelope(
            frame); // Validate before delivering notifications.
        _events?.add(frame);
      }
    } catch (error, stackTrace) {
      _fail(error, stackTrace);
    }
  }

  void _fail(Object error, [StackTrace? stackTrace]) {
    if (_closed) return;
    for (final pending in _pending.values) {
      pending.completeError(error, stackTrace);
    }
    _pending.clear();
    _events?.addError(error, stackTrace);
    unawaited(close());
  }

  @override
  Future<void> close() => _closing ??= _close();

  Future<void> _close() async {
    _closed = true;
    for (final pending in _pending.values) {
      pending.completeError(StateError('Dwo stdio transport is closed'));
    }
    _pending.clear();
    // A single-subscription stream may not have a listener yet. Do not await
    // controller.close(), which would otherwise prevent transport disposal.
    final events = _events;
    _events = null;
    if (events != null) unawaited(events.close());
    try {
      await _process.stdin.close().timeout(const Duration(seconds: 1));
      await _process.exitCode.timeout(const Duration(seconds: 1));
    } catch (_) {
      _process.kill();
      try {
        await _process.exitCode.timeout(const Duration(seconds: 1));
      } catch (_) {
        // OS termination is best-effort and must never hang client shutdown.
      }
    } finally {
      await _stdout.cancel();
      await _stderr.cancel();
    }
  }
}
