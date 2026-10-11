import { fileURLToPath } from 'node:url';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { connectCodex, type CodexClient, type v2 } from '@kratos/codex';

interface TurnRequest {
  method: string;
  params: { threadId: string; turnId?: string; input?: v2.UserInput[] };
}
// These controls belong to the fake process, not the generated Codex protocol.
interface FakeControls {
  request(method: 'test/turnRequests', params: object): Promise<TurnRequest[]>;
  request(method: string, params: unknown): Promise<unknown>;
}

const clients: CodexClient[] = [];
afterEach(async () => {
  await Promise.all(clients.splice(0).map((client) => client.close()));
  vi.doUnmock('@kratos/codex');
});

function input(text: string): v2.UserInput[] {
  return [{ type: 'text', text, text_elements: [] }];
}

function turn(id: string, status: v2.Turn['status'] = 'inProgress'): v2.Turn {
  return {
    id,
    status,
    items: [],
    itemsView: 'full',
    error: null,
    startedAt: null,
    completedAt: null,
    durationMs: null,
  };
}

async function setup() {
  const client = await connectCodex({
    command: process.execPath,
    args: [fileURLToPath(new URL('../../codex/test/fake-codex.mjs', import.meta.url))],
  });
  clients.push(client);
  const controls = client as unknown as FakeControls;
  // Only replace process creation. Requests and events use the real client transport.
  vi.resetModules();
  vi.doMock('@kratos/codex', () => ({ connectCodex: () => Promise.resolve(client) }));
  const provider = await import('../src/providers/codex');
  provider.setThreadPermissions('thread-A', 'askMe');
  provider.setThreadPermissions('thread-B', 'askMe');

  return {
    ...provider,
    requests: async (method: string) => {
      const requests = await controls.request('test/turnRequests', {});
      return requests.filter((request) => request.method === method).map((request) => request.params);
    },
    start: (turnId: string, threadId = 'thread-A', status: v2.Turn['status'] = 'inProgress') =>
      controls.request('test/settleTurn', { method: 'turn/start', threadId, result: { turn: turn(turnId, status) } }),
    interrupt: (error?: string, threadId = 'thread-A') =>
      controls.request('test/settleTurn', { method: 'turn/interrupt', threadId, result: {}, error }),
    failStart: () =>
      controls.request('test/settleTurn', { method: 'turn/start', threadId: 'thread-A', error: 'Start rejected' }),
    complete: (turnId: string, threadId = 'thread-A') =>
      controls.request('test/emit', {
        method: 'turn/completed',
        params: { threadId, turn: turn(turnId, 'interrupted') },
      }),
    crash: () => controls.request('test/exit', { code: 3 }).catch(() => undefined),
  };
}

describe('Codex turn stopping through the client transport', () => {
  it('keeps queued messages until completion, even after interruption is acknowledged', async () => {
    const provider = await setup();
    const sent = provider.sendMessage('thread-A', input('A'));
    await provider.start('turn-A');
    await sent;
    await provider.sendMessage('thread-A', input('B'));

    const stopped = provider.stopTurn('thread-A');
    const stoppedAgain = provider.stopTurn('thread-A');
    await provider.interrupt();
    await Promise.all([stopped, stoppedAgain]);
    await provider.stopTurn('thread-A');
    await provider.sendMessage('thread-A', input('C'));

    expect(await provider.requests('turn/interrupt')).toEqual([{ threadId: 'thread-A', turnId: 'turn-A' }]);
    expect(await provider.requests('turn/start')).toHaveLength(1);
    await provider.complete('turn-A');
    expect(await provider.requests('turn/start')).toMatchObject([
      { input: input('A') },
      { input: [...input('B'), ...input('C')] },
    ]);
    await provider.start('turn-B');
  });

  it('waits for the exact start response and shares repeated Stop requests', async () => {
    const provider = await setup();
    const sent = provider.sendMessage('thread-A', input('A'));
    const stopped = provider.stopTurn('thread-A');
    const stoppedAgain = provider.stopTurn('thread-A');
    await provider.sendMessage('thread-A', input('B'));
    expect(await provider.requests('turn/interrupt')).toEqual([]);

    await provider.start('turn-A');
    await sent;
    expect(await provider.requests('turn/interrupt')).toEqual([{ threadId: 'thread-A', turnId: 'turn-A' }]);
    await provider.interrupt();
    await Promise.all([stopped, stoppedAgain]);
    expect(await provider.requests('turn/start')).toHaveLength(1);

    await provider.complete('turn-A');
    expect(await provider.requests('turn/start')).toMatchObject([{ input: input('A') }, { input: input('B') }]);
    await provider.start('turn-B');
  });

  it('does not interrupt a completed turn or overwrite the next turn when completion precedes the start response', async () => {
    const provider = await setup();
    const sent = provider.sendMessage('thread-A', input('A'));
    const stopped = provider.stopTurn('thread-A');
    await provider.sendMessage('thread-A', input('B'));
    await provider.complete('turn-A');
    expect(await provider.requests('turn/start')).toHaveLength(1);

    await provider.start('turn-A');
    await Promise.all([sent, stopped]);
    expect(await provider.requests('turn/interrupt')).toEqual([]);
    expect(await provider.requests('turn/start')).toHaveLength(2);
    // An old completion must not finish B while its start response is pending, or afterward.
    await provider.complete('turn-A');
    await provider.start('turn-B');
    await provider.complete('turn-A');
    await provider.sendMessage('thread-A', input('C'));
    expect(await provider.requests('turn/start')).toHaveLength(2);
    const stopB = provider.stopTurn('thread-A');
    expect(await provider.requests('turn/interrupt')).toEqual([{ threadId: 'thread-A', turnId: 'turn-B' }]);
    await provider.interrupt();
    await stopB;
  });

  it('reports a rejected start to Stop without sending an interrupt', async () => {
    const provider = await setup();
    const sent = expect(provider.sendMessage('thread-A', input('A'))).rejects.toThrow('Start rejected');
    const stopped = expect(provider.stopTurn('thread-A')).rejects.toThrow('Start rejected');
    await provider.failStart();
    await Promise.all([sent, stopped]);
    expect(await provider.requests('turn/interrupt')).toEqual([]);

    const next = provider.sendMessage('thread-A', input('B'));
    await provider.start('turn-B');
    await next;
    expect(await provider.requests('turn/start')).toHaveLength(2);
  });

  it('allows another Stop after interruption fails, without releasing queued work', async () => {
    const provider = await setup();
    const sent = provider.sendMessage('thread-A', input('A'));
    await provider.start('turn-A');
    await sent;
    const stopped = expect(provider.stopTurn('thread-A')).rejects.toThrow('Interrupt rejected');
    await provider.sendMessage('thread-A', input('B'));
    await provider.interrupt('Interrupt rejected');
    await stopped;
    expect(await provider.requests('turn/start')).toHaveLength(1);

    const retry = provider.stopTurn('thread-A');
    expect(await provider.requests('turn/interrupt')).toEqual([
      { threadId: 'thread-A', turnId: 'turn-A' },
      { threadId: 'thread-A', turnId: 'turn-A' },
    ]);
    await provider.interrupt();
    await retry;
    await provider.complete('turn-A');
    expect(await provider.requests('turn/start')).toMatchObject([{ input: input('A') }, { input: input('B') }]);
    await provider.start('turn-B');
  });

  it.each([undefined, 'Late interrupt failure'])(
    'does not change the next turn after a late interrupt result: %s',
    async (error) => {
      const provider = await setup();
      const sent = provider.sendMessage('thread-A', input('A'));
      await provider.start('turn-A');
      await sent;
      const stopping = provider.stopTurn('thread-A');
      const stopped = error ? expect(stopping).rejects.toThrow(error) : expect(stopping).resolves.toBeUndefined();
      await provider.sendMessage('thread-A', input('B'));
      await provider.complete('turn-A');
      await provider.start('turn-B');
      await provider.interrupt(error);
      await stopped;

      await provider.sendMessage('thread-A', input('C'));
      expect(await provider.requests('turn/start')).toHaveLength(2);
      const stopB = provider.stopTurn('thread-A');
      expect(await provider.requests('turn/interrupt')).toEqual([
        { threadId: 'thread-A', turnId: 'turn-A' },
        { threadId: 'thread-A', turnId: 'turn-B' },
      ]);
      await provider.interrupt();
      await stopB;
    },
  );

  it('keeps stop requests separate when different threads start in reverse response order', async () => {
    const provider = await setup();
    const sentA = provider.sendMessage('thread-A', input('A'));
    const stoppedA = provider.stopTurn('thread-A');
    const sentB = provider.sendMessage('thread-B', input('B'));
    await provider.start('turn-B', 'thread-B');
    await sentB;
    expect(await provider.requests('turn/interrupt')).toEqual([]);
    await provider.start('turn-A');
    await sentA;
    expect(await provider.requests('turn/interrupt')).toEqual([{ threadId: 'thread-A', turnId: 'turn-A' }]);
    await provider.interrupt();
    await stoppedA;
  });

  it('skips interruption when the start response itself reports a terminal turn', async () => {
    const provider = await setup();
    const sent = provider.sendMessage('thread-A', input('A'));
    const stopped = provider.stopTurn('thread-A');
    await provider.start('turn-A', 'thread-A', 'completed');
    await Promise.all([sent, stopped]);
    expect(await provider.requests('turn/interrupt')).toEqual([]);
  });

  it('rejects Stop waiting on a start when the process exits', async () => {
    const provider = await setup();
    const sent = expect(provider.sendMessage('thread-A', input('A'))).rejects.toThrow();
    const stopped = expect(provider.stopTurn('thread-A')).rejects.toThrow();
    await provider.crash();
    await Promise.all([sent, stopped]);
  });
});
