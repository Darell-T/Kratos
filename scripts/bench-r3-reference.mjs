// Reference provider only: the TypeScript engine never connected its HTTP API to Codex.
import assert from 'node:assert/strict';
import { once } from 'node:events';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { createConnection, createServer } from 'node:net';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { PassThrough } from 'node:stream';
import { Store } from '../packages/engine/src/store.ts';
import { createCodexProvider } from '../packages/engine/src/providers/codex.ts';

const home = mkdtempSync(join(tmpdir(), 'kratos-reference-bench-'));
const controlFile = join(home, 'control');
const fake = resolve(process.argv[2] ?? 'target/release/fake-codex' + (process.platform === 'win32' ? '.exe' : ''));
const store = new Store(home);
const listener = createServer();
const peers = [];
let provider;

function peer(socket) {
  socket.setNoDelay(true);
  socket.setTimeout(10000, () => socket.destroy(new Error('fake-codex deadline exceeded')));
  const messages = new PassThrough({ objectMode: true });
  const lines = createInterface({ input: socket });
  lines.on('line', (line) => messages.write({ value: JSON.parse(line), at: performance.now() }));
  socket.on('error', (error) => messages.destroy(error));
  socket.on('close', () => messages.end());
  messages.on('error', () => {});
  const iterator = messages[Symbol.asyncIterator]();
  const result = {
    socket,
    async next() {
      const message = await iterator.next();
      assert.equal(message.done, false, 'fake-codex disconnected');
      return message.value;
    },
    async call(method, params) {
      socket.write(JSON.stringify({ id: 1, method, params }) + '\n');
      const { value } = await result.next();
      assert.equal(value.error, undefined, JSON.stringify(value));
      return value.result;
    },
  };
  peers.push(result);
  return result;
}

function report(samples) {
  samples.sort((a, b) => a - b);
  const percentile = (p) => Number(samples[Math.ceil((samples.length * p) / 100) - 1].toFixed(3));
  return {
    messages: samples.length,
    p50Ms: percentile(50),
    p90Ms: percentile(90),
    p95Ms: percentile(95),
    p99Ms: percentile(99),
    maxMs: percentile(100),
  };
}

try {
  provider = await createCodexProvider({ store, command: fake, args: ['--control-file', controlFile], cwd: home });
  const threads = [];
  for (let i = 0; i < 10; i++) threads.push(await provider.startThread(home));
  const controlSocket = createConnection({ host: '127.0.0.1', port: Number(readFileSync(controlFile, 'utf8')) });
  await once(controlSocket, 'connect');
  const control = peer(controlSocket);
  listener.listen(0, '127.0.0.1');
  await once(listener, 'listening');
  const accepted = once(listener, 'connection');
  await control.call('test/observeTurns', { port: listener.address().port });
  const observer = peer((await accepted)[0]);
  const dispatch = [];
  const completion = [];
  for (let i = 0; i < 210; i++) {
    const threadId = threads[i % 10];
    const id = `reference-${i}`;
    const start = performance.now();
    const sending = provider.sendMessage(threadId, [{ type: 'text', text: id, text_elements: [] }], id);
    const observed = await observer.next();
    assert.equal(observed.value.method, 'turn/start');
    assert.equal(observed.value.params.clientUserMessageId, id);
    const turn = { id: `turn-${i}`, status: 'inProgress', items: [] };
    await control.call('test/settleTurn', { method: 'turn/start', threadId, result: { turn } });
    await sending;
    let unsubscribe;
    const finished = new Promise((resolveFinished) => {
      unsubscribe = provider.onEvent((event) => {
        if (event.kind === 'turn.completed') resolveFinished({ event, at: performance.now() });
      });
    });
    const completing = performance.now();
    await control.call('test/emit', {
      method: 'turn/completed',
      params: { threadId, turn: { ...turn, status: 'completed' } },
    });
    const completed = await finished;
    unsubscribe();
    assert.equal(completed.event.turn.id, turn.id);
    assert.equal(completed.event.turn.status, 'completed');
    if (i >= 10) {
      dispatch.push(observed.at - start);
      completion.push(completed.at - completing);
    }
  }
  console.log(
    JSON.stringify({
      reference: 'TypeScript provider, without HTTP or WebSocket',
      threads: 10,
      warmupTurns: 10,
      dispatch: report(dispatch),
      completion: report(completion),
      nodeRssBytes: process.memoryUsage().rss,
    }),
  );
} finally {
  for (const { socket } of peers) socket.destroy();
  listener.close();
  await provider?.close();
  store.close();
  rmSync(home, { recursive: true, force: true });
}
