import 'dart:async';
import 'dart:io';

import 'package:dwo_rpc/dwo_rpc.dart';

void check(bool condition, String message) {
  if (!condition) throw StateError(message);
}

Future<void> fails<T>(Future<Object?> future) async {
  try {
    await future;
  } catch (error) {
    check(error is T, 'Expected $T, got $error');
    return;
  }
  throw StateError('Expected $T');
}

JsonObject request(String id, String method) => {
      'jsonrpc': '2.0',
      'route': 'dwo',
      'id': id,
      'method': method,
      'params': {},
    };

Future<void> main(List<String> args) async {
  if (args.length != 1)
    throw ArgumentError('Pass the compiled fixture executable');
  final executable = File(args.single).absolute.path;
  final transport = await DwoStdioTransport.connect(executable: executable);
  final client = DwoRpcClient(transport);
  try {
    check((await client.call('test.fragment'))['text'] == '你好🌍',
        'fragmented UTF-8/CRLF');
    await transport.request(request('duplicate', 'test.duplicate'));
    check((await client.call('test.stderr'))['ok'] == true,
        'stderr must drain separately');
    final slow = client.call('test.slow');
    check((await client.call('test.fast'))['method'] == 'test.fast',
        'out-of-order response correlation');
    check((await slow)['slow'] == true, 'pending slow response');
    final raw = await transport.subscribe(request('sub', 'event.subscribe'));
    check(raw.response['id'] == 'sub', 'subscription response correlation');
    final event = DwoEvent.fromEnvelope(await raw.events.first);
    check(event.seq == 2, 'immediate event was buffered');
    check((await client.daemonStatus())['method'] == 'daemon.status',
        'requests work after subscription');
    await fails<StateError>(
        transport.subscribe(request('sub2', 'event.subscribe')));
    try {
      await client.call('test.error');
      throw StateError('expected structured error');
    } on DwoRpcException catch (error) {
      check(error.code == 'conflict' && (error.data as Map)['field'] == 'name',
          'structured error preserved');
    }
    final pending = fails<StateError>(client.call('test.hang'));
    await Future.wait([client.close(), client.close(), pending]);
    await fails<StateError>(client.daemonStatus());
  } finally {
    await client.close();
  }
  final traffic = File('$executable.traffic').readAsLinesSync();
  check(traffic.last == 'EOF', 'bridge stdin closed gracefully');
  check(
      !traffic.contains('daemon.shutdown') &&
          !traffic.contains('session.cancel') &&
          !traffic.contains('session/cancel'),
      'close generated lifecycle RPC');

  final timed = await DwoStdioTransport.connect(
    executable: executable,
    timeout: const Duration(milliseconds: 500),
  );
  try {
    final late =
        fails<TimeoutException>(timed.request(request('late', 'test.slow')));
    await fails<StateError>(timed.request(request('late', 'test.fast')));
    await late;
    // Wait for the timed-out reply via another delayed request. The late reply
    // must not complete an unrelated pending request or poison the connection.
    await fails<TimeoutException>(timed.request(request('wait', 'test.slow')));
    check((await timed.request(request('after', 'test.fast')))['id'] == 'after',
        'late reply corrupted pending map');
  } finally {
    await timed.close();
  }

  for (final method in [
    'test.malformed',
    'test.invalid',
    'test.bad-event',
    'test.exit'
  ]) {
    final broken = await DwoStdioTransport.connect(executable: executable);
    final subscription =
        await broken.subscribe(request('events', 'event.subscribe'));
    final streamFailure = fails<Object>(subscription.events.drain<void>());
    final hanging =
        fails<Object>(broken.request(request('hanging', 'test.hang')));
    await fails<Object>(broken.request(request('break', method)));
    await hanging;
    await streamFailure;
    await broken.close().timeout(const Duration(seconds: 4));
  }

  // Closing must not await an event controller with no listener attached.
  final unlistened = await DwoStdioTransport.connect(executable: executable);
  await unlistened.subscribe(request('unlistened', 'event.subscribe'));
  await unlistened.close().timeout(const Duration(seconds: 4));

  for (final mode in ['hang-start', 'exit-start', 'hang-close']) {
    final copy = await File(executable)
        .copy('$executable-$mode${Platform.isWindows ? '.exe' : ''}');
    try {
      if (mode == 'hang-start') {
        await fails<TimeoutException>(DwoStdioTransport.connect(
          executable: copy.path,
          timeout: const Duration(milliseconds: 200),
        ));
      } else if (mode == 'exit-start') {
        await fails<StateError>(
            DwoStdioTransport.connect(executable: copy.path));
      } else {
        final stubborn = await DwoStdioTransport.connect(executable: copy.path);
        await stubborn.close().timeout(const Duration(seconds: 4));
      }
    } finally {
      await copy
          .delete(); // Also proves connect failure released the child executable.
      final log = File('${copy.path}.traffic');
      if (await log.exists()) await log.delete();
    }
  }
  await fails<ProcessException>(
      DwoStdioTransport.connect(executable: '$executable-missing'));
  stdout.writeln('stdio transport tests passed');
}
