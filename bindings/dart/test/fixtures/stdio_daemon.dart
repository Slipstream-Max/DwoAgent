// Compile this isolated JSON-line peer to an executable for transport tests.
// It never connects to a daemon or creates an agent runtime.
import 'dart:async';
import 'dart:convert';
import 'dart:io';

void send(Object value) => stdout.writeln(jsonEncode(value));

Future<void> main(List<String> arguments) async {
  if (arguments.length != 1 || arguments.single != 'rpc') exit(64);
  final executable = Platform.resolvedExecutable;
  final traffic = File('$executable.traffic');
  final frames = stdin.transform(utf8.decoder).transform(const LineSplitter());
  await for (final line in frames) {
    final request = jsonDecode(line) as Map<String, dynamic>;
    final method = request['method'] as String;
    traffic.writeAsStringSync('$method\n', mode: FileMode.append);
    final id = request['id'];
    Map<String, Object?> result(Object? value) => {
          'jsonrpc': '2.0',
          'id': id,
          'result': value,
        };
    switch (method) {
      case 'dwo.capabilities':
        if (executable.contains('hang-start')) continue;
        if (executable.contains('exit-start')) {
          stderr.writeln('fixture daemon unavailable');
          await stderr.flush();
          exit(7);
        }
        send(result({
          'protocolVersion': 3,
          'route': 'dwo',
          'requestIds': true,
          'structuredErrors': true,
          'eventCursor': true,
          'methods': <String>[],
          'methodSpecs': <Object>[],
          'events': <String>[],
        }));
      case 'event.subscribe':
        // Reply and immediate event in the same OS write to exercise buffering
        // before subscribe() returns and before a stream listener is attached.
        stdout.write('${jsonEncode(result({
              'cursor': 0,
              'nextCursor': 1,
              'oldestCursor': 1,
              'truncated': false,
              'events': <Object>[],
            }))}\n${jsonEncode({
              'jsonrpc': '2.0',
              'route': 'dwo',
              'method': 'config.changed',
              'params': {
                'seq': 2,
                'params': {'source': 'fixture'}
              },
            })}\n');
      case 'test.fragment':
        final bytes =
            utf8.encode('${jsonEncode(result({'text': '你好🌍'}))}\r\n');
        for (final byte in bytes) {
          stdout.add([byte]);
          await stdout.flush();
        }
      case 'test.slow':
        Timer(const Duration(milliseconds: 750),
            () => send(result({'slow': true})));
      case 'test.hang':
        continue;
      case 'test.duplicate':
        send(result(null));
        send(result(null));
      case 'test.error':
        send({
          'jsonrpc': '2.0',
          'id': id,
          'error': {
            'code': 'conflict',
            'message': 'fixture conflict',
            'data': {'field': 'name'},
          }
        });
      case 'test.malformed':
        stdout.writeln('{broken');
      case 'test.bad-event':
        send({'jsonrpc': '2.0', 'route': 'dwo', 'method': 'config.changed'});
      case 'test.invalid':
        send({'jsonrpc': '2.0', 'id': id, 'result': {}, 'error': {}});
      case 'test.exit':
        exit(9);
      case 'test.stderr':
        stderr.write('diagnostic only\n' * 10000);
        send(result({'ok': true}));
      default:
        send(result({'method': method, 'params': request['params']}));
    }
  }
  traffic.writeAsStringSync('EOF\n', mode: FileMode.append);
  if (executable.contains('hang-close')) {
    Timer.periodic(const Duration(seconds: 1), (_) {});
  }
}
