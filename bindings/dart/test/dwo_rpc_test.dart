import 'dart:async';

import 'package:dwo_rpc/dwo_rpc.dart';

final class FakeTransport implements DwoTransport {
  JsonObject? lastRequest;

  @override
  Future<JsonObject> request(JsonObject envelope) async {
    lastRequest = envelope;
    return {
      'jsonrpc': '2.0',
      'id': envelope['id'],
      'result': <String, Object?>{'healthy': true},
    };
  }

  @override
  Future<DwoRawSubscription> subscribe(JsonObject envelope) async {
    lastRequest = envelope;
    return DwoRawSubscription(
      {
        'jsonrpc': '2.0',
        'id': envelope['id'],
        'result': <String, Object?>{
          'cursor': 0,
          'nextCursor': 1,
          'oldestCursor': 1,
          'truncated': false,
          'events': [
            {
              'seq': 1,
              'event': 'config.changed',
              'params': <String, Object?>{},
            },
          ],
        },
      },
      Stream.value({
        'jsonrpc': '2.0',
        'route': 'dwo',
        'method': 'config.changed',
        'params': {
          'seq': 2,
          'params': {'source': 'profile'},
        },
      }),
    );
  }

  @override
  Future<void> close() async {}
}

final class ErrorTransport implements DwoTransport {
  @override
  Future<JsonObject> request(JsonObject envelope) async => {
        'jsonrpc': '2.0',
        'id': envelope['id'],
        'error': <String, Object?>{
          'code': 'invalid_params',
          'message': 'websocket.bind must be an IP address',
          'data': <String, Object?>{'field': 'bind'},
        },
      };

  @override
  Future<DwoRawSubscription> subscribe(JsonObject envelope) =>
      throw UnimplementedError();

  @override
  Future<void> close() async {}
}

Future<void> main() async {
  final transport = FakeTransport();
  final client = DwoRpcClient(transport);
  final status = await client.daemonStatus();
  assert(status['healthy'] == true);
  assert(transport.lastRequest?['route'] == 'dwo');

  final subscription = await client.subscribeEvents(cursor: 0);
  assert(subscription.replay.events.single.seq == 1);
  final event = await subscription.events.first;
  assert(event.seq == 2);
  assert(event.name == 'config.changed');
  assert(event.params['source'] == 'profile');

  await client.updateWebsocketConfig(
    {'enabled': true, 'bind': '127.0.0.1', 'port': 8787},
    requestId: 'websocket-config-1',
  );
  assert(transport.lastRequest?['method'] == 'websocket.config');
  assert(transport.lastRequest?['id'] == 'websocket-config-1');
  assert(
    (transport.lastRequest?['params'] as JsonObject)['port'] == 8787,
  );

  await client.setWebsocketEnabled(false, requestId: 'websocket-disable-1');
  assert(transport.lastRequest?['method'] == 'websocket.disable');

  try {
    await client.channel('websocket', 'status');
    throw StateError('websocket was accepted as a channel');
  } on ArgumentError catch (error) {
    assert(error.message.toString().contains('weixin'));
  }

  final errorClient = DwoRpcClient(ErrorTransport());
  try {
    await errorClient.updateWebsocketConfig(
      {'enabled': true, 'bind': 'localhost', 'port': 8787},
    );
    throw StateError('structured RPC error was not thrown');
  } on DwoRpcException catch (error) {
    assert(error.code == 'invalid_params');
    assert(error.message.contains('websocket.bind'));
    assert((error.data as JsonObject)['field'] == 'bind');
  }
}
