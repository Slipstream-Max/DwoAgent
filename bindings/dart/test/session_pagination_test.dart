import 'package:dwo_rpc/dwo_rpc.dart';

final class PageTransport implements DwoTransport {
  PageTransport(this.pages);
  final List<Object?> pages;
  final List<JsonObject> requests = [];

  @override
  Future<JsonObject> request(JsonObject envelope) async {
    requests.add(envelope);
    if (pages.isEmpty) throw StateError('Unexpected extra page request');
    return {
      'jsonrpc': '2.0',
      'id': envelope['id'],
      'result': pages.removeAt(0)
    };
  }

  @override
  Future<DwoRawSubscription> subscribe(JsonObject envelope) =>
      throw UnimplementedError();

  @override
  Future<void> close() async {}
}

void check(bool condition, String message) {
  if (!condition) throw StateError(message);
}

Future<void> main() async {
  final transport = PageTransport([
    {
      'sessions': [
        {'id': 'one'},
        {'id': 'two'}
      ],
      'nextCursor': 2
    },
    {
      'sessions': [
        {'id': 'three'}
      ],
      'nextCursor': null
    },
  ]);
  final sessions = await DwoRpcClient(transport).sessions(all: false, limit: 2);
  check(sessions.map((s) => s['id']).join(',') == 'one,two,three',
      'all pages returned in order');
  final first = transport.requests.first['params'] as JsonObject;
  final second = transport.requests.last['params'] as JsonObject;
  check(
      first['all'] == false &&
          first['limit'] == 2 &&
          !first.containsKey('cursor'),
      'first page filters');
  check(
      second['all'] == false && second['limit'] == 2 && second['cursor'] == '2',
      'numeric cursor converted to Host string parameter');
  check(transport.requests.first['id'] != transport.requests.last['id'],
      'distinct page request IDs');

  final pageTransport = PageTransport([
    {'sessions': <Object>[], 'nextCursor': 40},
  ]);
  final page =
      await DwoRpcClient(pageTransport).sessionsPage(cursor: 20, limit: 20);
  check(page.sessions.isEmpty && page.nextCursor == 40,
      'explicit page retains cursor');
  check((pageTransport.requests.single['params'] as Map)['cursor'] == '20',
      'explicit cursor encoding');

  final empty = DwoRpcClient(PageTransport([
    {'sessions': <Object>[], 'nextCursor': null}
  ]));
  check((await empty.sessions()).isEmpty, 'empty page');

  for (final pages in <List<Object?>>[
    [
      {'sessions': <Object>[], 'nextCursor': 2},
      {'sessions': <Object>[], 'nextCursor': 2}
    ],
    [
      {'sessions': <Object>[], 'nextCursor': 4},
      {'sessions': <Object>[], 'nextCursor': 2}
    ],
    [
      {'sessions': <Object>[], 'nextCursor': 'bad'}
    ],
    [
      {'sessions': <Object>[], 'nextCursor': -1}
    ],
    [
      {
        'sessions': [7],
        'nextCursor': null
      }
    ],
  ]) {
    try {
      await DwoRpcClient(PageTransport(pages)).sessions();
      throw StateError('malformed pagination accepted');
    } on FormatException {
      // Expected, including cursor cycles that would otherwise loop forever.
    }
  }
  for (final limit in [0, -1]) {
    try {
      await DwoRpcClient(PageTransport([])).sessions(limit: limit);
      throw StateError('invalid limit accepted');
    } on ArgumentError {
      // Rejected before any transport request.
    }
  }
  print('session pagination tests passed');
}
