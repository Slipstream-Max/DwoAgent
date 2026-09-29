import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:dwo_rpc/dwo_rpc.dart';

const deadline = Duration(seconds: 3);

void check(bool condition, String message) {
  if (!condition) throw StateError(message);
}

Future<Object> fails<T>(Future<Object?> future) async {
  try {
    await future.timeout(deadline);
  } catch (error) {
    check(error is T, 'Expected $T, got $error');
    return error;
  }
  throw StateError('Expected $T');
}

JsonObject request(String id, [String method = 'daemon.status']) => {
      'jsonrpc': '2.0',
      'route': 'dwo',
      'id': id,
      'method': method,
      'params': {},
    };

JsonObject event(int seq) => {
      'jsonrpc': '2.0',
      'route': 'dwo',
      'method': 'config.changed',
      'params': {'seq': seq, 'params': <String, Object?>{}},
    };

final class LoopbackServer {
  LoopbackServer._(this.http) : incoming = StreamIterator(http);

  final HttpServer http;
  final StreamIterator<HttpRequest> incoming;
  WebSocket? socket;
  StreamIterator<dynamic>? messages;
  final disconnected = Completer<void>();

  static Future<LoopbackServer> start() async =>
      LoopbackServer._(await HttpServer.bind(InternetAddress.loopbackIPv4, 0));

  Uri get endpoint => Uri.parse('ws://127.0.0.1:${http.port}/dwo');

  Future<HttpRequest> handshake() async {
    check(await incoming.moveNext().timeout(deadline), 'missing handshake');
    final value = incoming.current;
    check(value.uri.path == '/dwo', 'wrong endpoint');
    check(value.headers.value(HttpHeaders.authorizationHeader) == 'Bearer test',
        'missing management token');
    return value;
  }

  Future<void> upgrade(HttpRequest request) async {
    final peer = socket = await WebSocketTransformer.upgrade(request);
    final frames = StreamController<dynamic>();
    messages = StreamIterator(frames.stream);
    peer.listen(frames.add, onError: frames.addError, onDone: () {
      disconnected.complete();
      unawaited(frames.close());
    });
  }

  Future<DwoWebSocketTransport> connect(
      {Duration timeout = const Duration(seconds: 10)}) async {
    final connecting = DwoWebSocketTransport.connect(endpoint,
        managementToken: 'test', timeout: timeout);
    await upgrade(await handshake());
    return connecting;
  }

  Future<JsonObject> next() async {
    check(await messages!.moveNext().timeout(deadline), 'missing request');
    return Map<String, Object?>.from(
        jsonDecode(messages!.current as String) as Map);
  }

  void send(Object frame) => socket!.add(jsonEncode(frame));

  void reply(JsonObject envelope, [JsonObject result = const {'ok': true}]) =>
      send({'jsonrpc': '2.0', 'id': envelope['id'], 'result': result});

  Future<void> close() async {
    await socket?.close();
    await messages?.cancel();
    await incoming.cancel();
    await http.close(force: true);
  }
}

Future<void> correlationAndReplay() async {
  final server = await LoopbackServer.start();
  final transport = await server.connect();
  try {
    await fails<ArgumentError>(transport.request(request('')));
    await fails<JsonUnsupportedObjectError>(transport.request({
      ...request('encode'),
      'params': Object(),
    }));
    final encoded = transport.request(request('encode'));
    server.reply(await server.next());
    await encoded;

    final slow = transport.request(request('slow'));
    final slowRequest = await server.next();
    await fails<StateError>(transport.request(request('slow')));
    final fast = transport.request(request('fast'));
    final fastRequest = await server.next();
    server.reply(fastRequest);
    server.reply(fastRequest); // Duplicate while another request is pending.
    server.reply(request('unknown'));
    server.reply(slowRequest);
    check((await fast)['id'] == 'fast', 'fast correlation');
    check((await slow)['id'] == 'slow', 'duplicate poisoned pending request');

    await fails<ArgumentError>(transport.subscribe(request('invalid')));
    final client = DwoRpcClient(transport);
    final subscribing = client.subscribeEvents(cursor: 0);
    final subscriptionRequest = await server.next();
    await fails<StateError>(
        transport.subscribe(request('concurrent', 'event.subscribe')));
    server.send(event(2)); // Arrives before the subscription reply.
    server.reply(subscriptionRequest, {
      'cursor': 0,
      'nextCursor': 1,
      'oldestCursor': 1,
      'truncated': false,
      'events': [
        {'seq': 1, 'event': 'config.changed', 'params': <String, Object?>{}},
      ],
    });
    server.send(event(3));
    final subscription = await subscribing;
    check(subscription.replay.events.single.seq == 1, 'replay was lost');
    // Round-trip before listening to ensure both live frames are buffered.
    final status = client.daemonStatus();
    server.reply(await server.next());
    await status;
    final events = <int>[];
    final received = Completer<void>();
    final listener = subscription.events.listen((value) {
      events.add(value.seq);
      if (events.length == 2) received.complete();
    });
    await received.future.timeout(deadline);
    check(events.join(',') == '2,3', 'live events lost or reordered');
    // Host cancels the event listener before closing its transport.
    await listener.cancel().timeout(deadline);
    await fails<StateError>(
        transport.subscribe(request('second', 'event.subscribe')));
    server.send(event(4));
    final afterCancel = client.daemonStatus();
    server.reply(await server.next());
    await afterCancel;
    final pending = fails<StateError>(transport.request(request('pending')));
    await server.next();
    await Future.wait([transport.close(), transport.close(), pending])
        .timeout(deadline);
    await transport.close().timeout(deadline);
    await fails<StateError>(client.daemonStatus());
    await fails<StateError>(client.subscribeEvents());
    await server.disconnected.future.timeout(deadline);
  } finally {
    await transport.close();
    await server.close();
  }
}

Future<void> terminalFailure(String mode) async {
  final server = await LoopbackServer.start();
  final transport = await server.connect();
  try {
    final subscribing = transport.subscribe(request('sub', 'event.subscribe'));
    server.reply(await server.next());
    final subscription = await subscribing;
    final errors = <Object>[];
    final done = Completer<void>();
    subscription.events.listen((_) {}, onError: (Object error) {
      errors.add(error);
    }, onDone: done.complete);
    final first = fails<Object>(transport.request(request('first')));
    final second = fails<Object>(transport.request(request('second')));
    await server.next();
    await server.next();
    if (mode == 'eof') {
      await server.socket!.close();
    } else if (mode == 'binary') {
      server.socket!.add([1, 2, 3]);
    } else {
      server.socket!.add(mode == 'json' ? '{' : '[]');
      server.socket!.add('{'); // Further bad frames must not fail twice.
    }
    final failures = await Future.wait([first, second]);
    await done.future.timeout(deadline);
    check(errors.length == 1, '$mode must emit exactly one terminal error');
    check(
        identical(failures[0], errors.single) &&
            identical(failures[1], errors.single),
        'terminal cause must be shared');
    check(
        errors.single is! TimeoutException, 'pending calls waited for timeout');
    check(
        mode == 'eof'
            ? errors.single is StateError
            : errors.single is FormatException,
        'wrong terminal error for $mode');
    await fails<StateError>(transport.request(request('closed')));
    await Future.wait([transport.close(), transport.close()]).timeout(deadline);
  } finally {
    await transport.close();
    await server.close();
  }
}

Future<void> closeWithoutConsuming(String mode) async {
  final server = await LoopbackServer.start();
  final transport = await server.connect();
  try {
    final subscribing = transport.subscribe(request('sub', 'event.subscribe'));
    server.reply(await server.next());
    final subscription = await subscribing;
    StreamSubscription<JsonObject>? listener;
    final done = Completer<void>();
    if (mode == 'paused') {
      listener = subscription.events.listen((_) {}, onDone: done.complete);
      listener.pause();
    }
    await Future.wait([transport.close(), transport.close()]).timeout(deadline);
    if (listener != null) {
      listener.resume();
      await done.future.timeout(deadline);
      await listener.cancel();
    } else {
      await subscription.events.drain<void>().timeout(deadline);
    }
  } finally {
    await transport.close();
    await server.close();
  }
}

Future<void> timeoutAndLateReply() async {
  final server = await LoopbackServer.start();
  final transport = await server.connect(timeout: const Duration(seconds: 1));
  try {
    final timed = fails<TimeoutException>(transport.request(request('timed')));
    final old = await server.next();
    await timed;
    final next = transport.request(request('next'));
    final current = await server.next();
    server.reply(old);
    server.reply(current);
    check((await next)['id'] == 'next', 'late reply corrupted pending calls');

    final subscribing = fails<TimeoutException>(
        transport.subscribe(request('sub', 'event.subscribe')));
    await server.next();
    await subscribing; // Failed subscribe has no event listener to drain it.
    await transport.close().timeout(deadline);
    await fails<StateError>(transport.request(request('closed')));
  } finally {
    await transport.close();
    await server.close();
  }
}

Future<void> connectTimeout() async {
  for (final lateUpgrade in [true, false]) {
    final server = await LoopbackServer.start();
    try {
      final connecting = fails<TimeoutException>(DwoWebSocketTransport.connect(
          server.endpoint,
          managementToken: 'test',
          timeout: const Duration(milliseconds: 300)));
      final handshake = await server.handshake();
      await connecting;
      if (lateUpgrade) {
        await server.upgrade(handshake);
        await server.disconnected.future.timeout(deadline);
      } else {
        handshake.response.statusCode = HttpStatus.forbidden;
        await handshake.response.close();
      }
    } finally {
      await server.close();
    }
  }
}

Future<void> subscriptionFailure(String mode) async {
  final server = await LoopbackServer.start();
  final transport = await server.connect();
  try {
    final subscribing = DwoRpcClient(transport).subscribeEvents();
    final failure = mode == 'error'
        ? fails<DwoRpcException>(subscribing)
        : fails<StateError>(subscribing);
    final envelope = await server.next();
    if (mode == 'error') {
      server.send({
        'jsonrpc': '2.0',
        'id': envelope['id'],
        'error': {'code': 'conflict', 'message': 'subscription rejected'},
      });
    } else if (mode == 'eof') {
      await server.socket!.close();
    } else {
      await transport.close().timeout(deadline);
    }
    await failure;
    // No raw subscription was returned, so nobody can consume its stream.
    await Future.wait([transport.close(), transport.close()]).timeout(deadline);
  } finally {
    await transport.close();
    await server.close();
  }
}

Future<void> main() async {
  await fails<ArgumentError>(DwoWebSocketTransport.connect(
      Uri.parse('ws://127.0.0.1/invalid'),
      managementToken: 'test'));
  for (final timeout in [Duration.zero, const Duration(milliseconds: -1)]) {
    await fails<ArgumentError>(DwoWebSocketTransport.connect(
        Uri.parse('ws://127.0.0.1/dwo'),
        managementToken: 'test',
        timeout: timeout));
  }
  await correlationAndReplay();
  for (final mode in ['eof', 'json', 'object', 'binary']) {
    await terminalFailure(mode);
  }
  for (final mode in ['unlistened', 'paused']) {
    await closeWithoutConsuming(mode);
  }
  await timeoutAndLateReply();
  await connectTimeout();
  for (final mode in ['error', 'eof', 'close']) {
    await subscriptionFailure(mode);
  }
  stdout.writeln('websocket transport tests passed');
}
