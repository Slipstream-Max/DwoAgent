library;

import 'dart:async';
import 'dart:convert';
import 'dart:io';

typedef JsonObject = Map<String, Object?>;

abstract interface class DwoTransport {
  Future<JsonObject> request(JsonObject envelope);

  Future<DwoRawSubscription> subscribe(JsonObject envelope);

  Future<void> close();
}

final class DwoRawSubscription {
  const DwoRawSubscription(this.response, this.events);

  final JsonObject response;
  final Stream<JsonObject> events;
}

final class DwoWebSocketTransport implements DwoTransport {
  DwoWebSocketTransport._(this._socket, this._timeout) {
    _socket.listen(
      _onMessage,
      onError: _onError,
      onDone: _onDone,
      cancelOnError: true,
    );
  }

  final WebSocket _socket;
  final Duration _timeout;
  final Map<String, Completer<JsonObject>> _pending = {};
  final List<StreamController<JsonObject>> _subscriptions = [];
  bool _closed = false;

  static Future<DwoWebSocketTransport> connect(
    Uri endpoint, {
    required String managementToken,
    Duration timeout = const Duration(seconds: 30),
  }) async {
    if (endpoint.path != '/dwo') {
      throw ArgumentError.value(endpoint, 'endpoint', 'path must be /dwo');
    }
    final socket = await WebSocket.connect(
      endpoint.toString(),
      headers: {'Authorization': 'Bearer $managementToken'},
    );
    return DwoWebSocketTransport._(socket, timeout);
  }

  @override
  Future<JsonObject> request(JsonObject envelope) async {
    if (_closed) {
      throw StateError('Dwo WebSocket transport is closed');
    }
    final id = envelope['id'];
    if (id is! String || id.isEmpty) {
      throw ArgumentError('Dwo request id must be a non-empty string');
    }
    if (_pending.containsKey(id)) {
      throw StateError('Dwo request id is already pending: $id');
    }
    final completer = Completer<JsonObject>();
    _pending[id] = completer;
    _socket.add(jsonEncode(envelope));
    try {
      return await completer.future.timeout(_timeout);
    } finally {
      _pending.remove(id);
    }
  }

  @override
  Future<DwoRawSubscription> subscribe(JsonObject envelope) async {
    late final StreamController<JsonObject> controller;
    controller = StreamController<JsonObject>(
      onCancel: () {
        _subscriptions.remove(controller);
      },
    );
    _subscriptions.add(controller);
    try {
      final response = await request(envelope);
      return DwoRawSubscription(response, controller.stream);
    } catch (_) {
      _subscriptions.remove(controller);
      await controller.close();
      rethrow;
    }
  }

  void _onMessage(Object? message) {
    try {
      final decoded = jsonDecode(message as String);
      if (decoded is! Map) {
        throw const FormatException('Dwo frame must be a JSON object');
      }
      final frame = Map<String, Object?>.from(decoded);
      final id = frame['id'];
      if (id is String) {
        _pending[id]?.complete(frame);
        return;
      }
      for (final subscription in List.of(_subscriptions)) {
        subscription.add(frame);
      }
    } catch (error, stackTrace) {
      _failAll(error, stackTrace);
    }
  }

  void _onError(Object error, StackTrace stackTrace) {
    _failAll(error, stackTrace);
  }

  void _onDone() {
    _closed = true;
    _failAll(StateError('Dwo WebSocket closed'), StackTrace.current);
  }

  void _failAll(Object error, StackTrace stackTrace) {
    for (final completer in _pending.values) {
      if (!completer.isCompleted) {
        completer.completeError(error, stackTrace);
      }
    }
    for (final subscription in List.of(_subscriptions)) {
      subscription.addError(error, stackTrace);
    }
  }

  @override
  Future<void> close() async {
    if (_closed) return;
    _closed = true;
    await _socket.close();
    for (final subscription in List.of(_subscriptions)) {
      await subscription.close();
    }
    _subscriptions.clear();
  }
}

final class DwoRpcException implements Exception {
  const DwoRpcException(this.code, this.message, [this.data]);

  final String code;
  final String message;
  final Object? data;

  @override
  String toString() => 'DwoRpcException($code): $message';
}

final class DwoMethodSpec {
  const DwoMethodSpec({
    required this.name,
    required this.route,
    required this.operation,
    required this.sideEffect,
    this.event,
  });

  factory DwoMethodSpec.fromJson(JsonObject json) => DwoMethodSpec(
        name: json.string('name'),
        route: json.string('route'),
        operation: json.string('operation'),
        sideEffect: json.boolean('sideEffect'),
        event: json['event'] as String?,
      );

  final String name;
  final String route;
  final String operation;
  final bool sideEffect;
  final String? event;
}

final class DwoCapabilities {
  const DwoCapabilities({
    required this.protocolVersion,
    required this.route,
    required this.requestIds,
    required this.structuredErrors,
    required this.eventCursor,
    required this.methods,
    required this.methodSpecs,
    required this.events,
  });

  factory DwoCapabilities.fromJson(JsonObject json) => DwoCapabilities(
        protocolVersion: json.integer('protocolVersion'),
        route: json.string('route'),
        requestIds: json.boolean('requestIds'),
        structuredErrors: json.boolean('structuredErrors'),
        eventCursor: json.boolean('eventCursor'),
        methods: json.stringList('methods'),
        methodSpecs:
            json.objectList('methodSpecs').map(DwoMethodSpec.fromJson).toList(),
        events: json.stringList('events'),
      );

  final int protocolVersion;
  final String route;
  final bool requestIds;
  final bool structuredErrors;
  final bool eventCursor;
  final List<String> methods;
  final List<DwoMethodSpec> methodSpecs;
  final List<String> events;
}

final class DwoEvent {
  const DwoEvent({required this.seq, required this.name, required this.params});

  factory DwoEvent.fromReplay(JsonObject json) => DwoEvent(
        seq: json.integer('seq'),
        name: json.string('event'),
        params: json.object('params'),
      );

  factory DwoEvent.fromEnvelope(JsonObject json) {
    if (json['jsonrpc'] != '2.0' || json['route'] != 'dwo') {
      throw const FormatException('Invalid Dwo event envelope');
    }
    final payload = json.object('params');
    return DwoEvent(
      seq: payload.integer('seq'),
      name: json.string('method'),
      params: payload.object('params'),
    );
  }

  final int seq;
  final String name;
  final JsonObject params;
}

final class DwoEventReplay {
  const DwoEventReplay({
    required this.cursor,
    required this.nextCursor,
    required this.oldestCursor,
    required this.truncated,
    required this.events,
  });

  factory DwoEventReplay.fromJson(JsonObject json) => DwoEventReplay(
        cursor: json.integer('cursor'),
        nextCursor: json.integer('nextCursor'),
        oldestCursor: json.integer('oldestCursor'),
        truncated: json.boolean('truncated'),
        events: json.objectList('events').map(DwoEvent.fromReplay).toList(),
      );

  final int cursor;
  final int nextCursor;
  final int oldestCursor;
  final bool truncated;
  final List<DwoEvent> events;
}

final class DwoEventSubscription {
  const DwoEventSubscription(this.replay, this.events);

  final DwoEventReplay replay;
  final Stream<DwoEvent> events;
}

final class DwoRpcClient {
  DwoRpcClient(this.transport);

  final DwoTransport transport;
  int _requestCounter = 0;

  Future<JsonObject> call(
    String method, {
    JsonObject params = const {},
    String? requestId,
  }) async {
    final id = requestId ?? _nextRequestId();
    return _asObject(await callValue(method, params: params, requestId: id));
  }

  Future<Object?> callValue(
    String method, {
    JsonObject params = const {},
    String? requestId,
  }) async {
    final id = requestId ?? _nextRequestId();
    final response = await transport.request(_request(id, method, params));
    return _resultValue(response, id);
  }

  Future<DwoCapabilities> capabilities() async =>
      DwoCapabilities.fromJson(await call('dwo.capabilities'));

  Future<JsonObject> daemonStatus() => call('daemon.status');

  Future<JsonObject> configSnapshot() => call('config.snapshot');

  Future<JsonObject> updateConfig(JsonObject update, {String? requestId}) =>
      call('config.update', params: update, requestId: requestId);

  Future<List<JsonObject>> sessions({bool all = true}) async =>
      _objectArray(await callValue('session.list', params: {'all': all}));

  Future<JsonObject> sessionStatus(String sessionId) =>
      call('session.status', params: {'session_id': sessionId});

  Future<JsonObject> sessionSnapshot(String sessionId) =>
      call('session.snapshot', params: {'session_id': sessionId});

  Future<JsonObject> models() => call('model.list');

  Future<JsonObject> setDefaultModel(
    String model, {
    String? reasoning,
    String? requestId,
  }) =>
      call(
        'model.set_default',
        params: {'model': model, if (reasoning != null) 'reasoning': reasoning},
        requestId: requestId,
      );

  Future<JsonObject> upsertModel(
    String provider,
    String name,
    JsonObject model, {
    String? requestId,
  }) =>
      call(
        'model.upsert',
        params: {'provider': provider, 'name': name, 'model': model},
        requestId: requestId,
      );

  Future<JsonObject> removeModel(
    String provider,
    String modelId, {
    String? requestId,
  }) =>
      call(
        'model.remove',
        params: {'provider': provider, 'modelId': modelId},
        requestId: requestId,
      );

  Future<JsonObject> modelCatalog() => call('model.catalog.list');

  Future<JsonObject> upsertModelFamily(
    String family,
    JsonObject spec, {
    String? requestId,
  }) =>
      call(
        'model.catalog.upsert',
        params: {'family': family, 'spec': spec},
        requestId: requestId,
      );

  Future<JsonObject> removeModelFamily(
    String family, {
    String? requestId,
  }) =>
      call(
        'model.catalog.remove',
        params: {'family': family},
        requestId: requestId,
      );

  Future<JsonObject> providers() => call('provider.list');

  Future<JsonObject> upsertProvider(
    String name,
    JsonObject provider, {
    String? requestId,
  }) =>
      call(
        'provider.upsert',
        params: {'name': name, 'provider': provider},
        requestId: requestId,
      );

  Future<JsonObject> removeProvider(String name, {String? requestId}) =>
      call('provider.remove', params: {'name': name}, requestId: requestId);

  Future<JsonObject> skills() => call('skill.list');

  Future<JsonObject> installSkill(
    String name,
    String content, {
    String? requestId,
  }) =>
      call(
        'skill.install',
        params: {'name': name, 'content': content},
        requestId: requestId,
      );

  Future<JsonObject> setSkillEnabled(
    String name,
    bool enabled, {
    String? requestId,
  }) =>
      call(
        enabled ? 'skill.enable' : 'skill.disable',
        params: {'name': name},
        requestId: requestId,
      );

  Future<JsonObject> uninstallSkill(String name, {String? requestId}) =>
      call('skill.uninstall', params: {'name': name}, requestId: requestId);

  Future<JsonObject> mcpServers() => call('mcp.list');

  Future<JsonObject> mcpConfig() => call('mcp.config');

  Future<JsonObject> installMcp(JsonObject params, {String? requestId}) =>
      call('mcp.install', params: params, requestId: requestId);

  Future<JsonObject> setMcpEnabled(
    String server,
    bool enabled, {
    String? requestId,
  }) =>
      call(
        enabled ? 'mcp.enable' : 'mcp.disable',
        params: {'server': server},
        requestId: requestId,
      );

  Future<JsonObject> authenticateMcp(
    String server, {
    bool authorized = true,
    String? requestId,
  }) =>
      call(
        authorized ? 'mcp.auth.login' : 'mcp.auth.unauth',
        params: {'server': server},
        requestId: requestId,
      );

  Future<List<JsonObject>> automations() async =>
      _objectArray(await callValue('automation.list'));

  Future<JsonObject> addAutomation(JsonObject job, {String? requestId}) =>
      call('automation.add', params: {'job': job}, requestId: requestId);

  Future<JsonObject> updateAutomation(
    String name,
    JsonObject update, {
    String? requestId,
  }) =>
      call(
        'automation.update',
        params: {'name': name, ...update},
        requestId: requestId,
      );

  Future<JsonObject> runAutomation(String job, {String? requestId}) =>
      call('automation.run', params: {'job': job}, requestId: requestId);

  Future<JsonObject> websocketStatus() => call('websocket.status');

  Future<JsonObject> websocketConfig() => call('websocket.config');

  Future<JsonObject> updateWebsocketConfig(
    JsonObject config, {
    String? requestId,
  }) =>
      call('websocket.config', params: config, requestId: requestId);

  Future<JsonObject> setWebsocketEnabled(
    bool enabled, {
    String? requestId,
  }) =>
      call(
        enabled ? 'websocket.enable' : 'websocket.disable',
        requestId: requestId,
      );

  Future<JsonObject> websocketToken() => call('websocket.token');

  Future<JsonObject> resetWebsocketToken({String? requestId}) =>
      call('websocket.reset_token', requestId: requestId);

  Future<List<JsonObject>> channels() async =>
      _objectArray(await callValue('channel.list'));

  Future<JsonObject> channel(
    String kind,
    String action, {
    JsonObject params = const {},
    String? requestId,
  }) {
    const kinds = {'weixin', 'telegram', 'feishu', 'qq'};
    if (!kinds.contains(kind)) {
      throw ArgumentError.value(
        kind,
        'kind',
        'must be one of: weixin, telegram, feishu, qq',
      );
    }
    return call('channel.$kind.$action', params: params, requestId: requestId);
  }

  Future<DwoEventReplay> readEvents({
    int? cursor,
    int limit = 50,
    String? event,
  }) async =>
      DwoEventReplay.fromJson(
        await call(
          'event.read',
          params: {
            if (cursor != null) 'cursor': cursor,
            'limit': limit,
            if (event != null) 'event': event,
          },
        ),
      );

  Future<DwoEventSubscription> subscribeEvents({
    int? cursor,
    int limit = 50,
    String? event,
  }) async {
    final id = _nextRequestId();
    final raw = await transport.subscribe(
      _request(id, 'event.subscribe', {
        if (cursor != null) 'cursor': cursor,
        'limit': limit,
        if (event != null) 'event': event,
      }),
    );
    final replay = DwoEventReplay.fromJson(_result(raw.response, id));
    final events = raw.events
        .where((frame) => frame['method'] != null)
        .map(DwoEvent.fromEnvelope)
        .where((value) => event == null || value.name == event);
    return DwoEventSubscription(replay, events);
  }

  Future<void> close() => transport.close();

  JsonObject _request(String id, String method, JsonObject params) => {
        'jsonrpc': '2.0',
        'id': id,
        'route': 'dwo',
        'method': method,
        'params': params,
      };

  Object? _resultValue(JsonObject response, String requestId) {
    if (response['jsonrpc'] != '2.0' || response['id'] != requestId) {
      throw const FormatException('Dwo response does not match the request');
    }
    final error = response['error'];
    if (error is Map) {
      final value = Map<String, Object?>.from(error);
      throw DwoRpcException(
        value.string('code'),
        value.string('message'),
        value['data'],
      );
    }
    return response['result'];
  }

  JsonObject _result(JsonObject response, String requestId) =>
      _asObject(_resultValue(response, requestId));

  JsonObject _asObject(Object? value) {
    if (value is Map) return Map<String, Object?>.from(value);
    throw const FormatException('RPC result is not an object');
  }

  String _nextRequestId() =>
      'flutter-${DateTime.now().microsecondsSinceEpoch}-${_requestCounter++}';
}

extension on JsonObject {
  String string(String key) {
    final value = this[key];
    if (value is String) return value;
    throw FormatException('$key must be a string');
  }

  int integer(String key) {
    final value = this[key];
    if (value is int) return value;
    throw FormatException('$key must be an integer');
  }

  bool boolean(String key) {
    final value = this[key];
    if (value is bool) return value;
    throw FormatException('$key must be a boolean');
  }

  JsonObject object(String key) {
    final value = this[key];
    if (value is Map) return Map<String, Object?>.from(value);
    throw FormatException('$key must be an object');
  }

  List<JsonObject> objectList(String key) {
    final value = this[key];
    if (value is! List) throw FormatException('$key must be an array');
    return value.map((item) {
      if (item is! Map) throw FormatException('$key must contain objects');
      return Map<String, Object?>.from(item);
    }).toList();
  }

  List<String> stringList(String key) {
    final value = this[key];
    if (value is! List || value.any((item) => item is! String)) {
      throw FormatException('$key must be an array of strings');
    }
    return value.cast<String>();
  }
}

List<JsonObject> _objectArray(Object? value) {
  if (value is List) {
    return value.map((item) {
      if (item is! Map)
        throw const FormatException('RPC result must contain objects');
      return Map<String, Object?>.from(item);
    }).toList();
  }
  throw const FormatException('RPC result is not an object array');
}
