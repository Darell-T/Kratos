import { describe, expect, it, vi } from 'vitest';
import { input, setup, turn } from './codex-fixture';

async function active(provider: Awaited<ReturnType<typeof setup>>) {
  const sent = provider.sendMessage('thread-A', input('A'), 'message-A');
  await provider.start('turn-A');
  await sent;
}

describe('Codex message delivery', () => {
  it('runs queued submissions individually in their original order', async () => {
    const provider = await setup();
    await active(provider);
    await provider.sendMessage('thread-A', input('B'), 'message-B');
    await provider.sendMessage('thread-A', input('C'), 'message-C');
    const idle = provider.waitUntilIdle('thread-A').catch((error: unknown) => error);
    await provider.complete('turn-A');
    expect(await provider.requests()).toMatchObject([{ input: input('A') }, { input: input('B') }]);
    await provider.start('turn-B');
    await provider.complete('turn-B');
    expect(await provider.requests()).toMatchObject([
      { input: input('A') },
      { input: input('B') },
      { input: input('C') },
    ]);
    await provider.start('turn-C');
    await provider.complete('turn-C');
    expect(await idle).toBeUndefined();
  });

  it('retries a known pre-execution rejection once and retains it after the second failure', async () => {
    const provider = await setup();
    const failed = expect(provider.sendMessage('thread-A', input('A'), 'message-A')).rejects.toThrow('Start rejected');
    await provider.sendMessage('thread-A', input('B'), 'message-B');
    await provider.failStart(-32602);
    await provider.failStart(-32602);
    await failed;
    await provider.sendMessage('thread-A', input('C'), 'message-C');
    expect(await provider.requests()).toHaveLength(2);
    expect(provider.messages('thread-A')).toMatchObject([
      { id: 'message-A', status: 'failed' },
      { id: 'message-B', status: 'queued' },
      { id: 'message-C', status: 'queued' },
    ]);
    expect(provider.removeQueuedMessage('thread-A', 'message-A')).toBe(true);
    await provider.start('turn-B');
    expect(await provider.requests()).toMatchObject([
      { input: input('A') },
      { input: input('A') },
      { input: input('B') },
    ]);
  });

  it('does not replay an uncertain delivery or let newer messages overtake it', async () => {
    const provider = await setup();
    const failed = expect(provider.sendMessage('thread-A', input('A'), 'message-A')).rejects.toThrow('Start rejected');
    await provider.failStart();
    await failed;
    const next = await provider.sendMessage('thread-A', input('B'), 'message-B');
    expect(next.status).toBe('queued');
    expect(await provider.requests()).toHaveLength(1);
    expect(provider.messages('thread-A')[0]).toMatchObject({
      id: 'message-A',
      status: 'unknown',
      error: 'Start rejected',
    });
    await expect(provider.retryMessage('thread-A', 'message-A')).rejects.toThrow('confirmed');
  });

  it('can discard uncertain delivery and explicitly resume retained work', async () => {
    const provider = await setup();
    const failed = expect(provider.sendMessage('thread-A', input('A'), 'message-A')).rejects.toThrow('Start rejected');
    await provider.failStart();
    await failed;
    await provider.sendMessage('thread-A', input('B'), 'message-B');
    expect((await provider.discardMessage('thread-A', 'message-A')).status).toBe('cancelled');
    expect(await provider.requests()).toHaveLength(1);
    const resumed = provider.resumeQueue('thread-A');
    await provider.start('turn-B');
    await resumed;
    expect(provider.messages('thread-A')[1]).toMatchObject({ status: 'accepted', turnId: 'turn-B' });
  });

  it('records delivery found in native history instead of discarding it', async () => {
    const provider = await setup();
    provider.store.addCodexMessage('message-A', 'thread-A', input('A'));
    provider.store.updateCodexMessage('message-A', 'unknown');
    await provider.control('test/setThread', {
      thread: {
        id: 'thread-A',
        cwd: 'C:\\project',
        status: { type: 'idle' },
        turns: [{ ...turn('accepted-turn', 'completed'), items: [{ type: 'userMessage', clientId: 'message-A' }] }],
      },
    });
    expect(await provider.discardMessage('thread-A', 'message-A')).toMatchObject({
      status: 'accepted',
      turnId: 'accepted-turn',
    });
    expect(await provider.requests()).toEqual([]);
  });

  it('keeps uncertain delivery unresolved when the native thread is active', async () => {
    const provider = await setup();
    provider.store.addCodexMessage('message-A', 'thread-A', input('A'));
    provider.store.updateCodexMessage('message-A', 'unknown');
    await provider.control('test/setThread', {
      thread: {
        id: 'thread-A',
        cwd: 'C:\\project',
        status: { type: 'active', activeFlags: [] },
        turns: [turn('turn-A')],
      },
    });
    await expect(provider.discardMessage('thread-A', 'message-A')).rejects.toThrow('must be idle');
    expect(provider.messages('thread-A')[0]?.status).toBe('unknown');
  });

  it('deduplicates submissions by ID without merging identical text', async () => {
    const provider = await setup();
    const first = provider.sendMessage('thread-A', input('Same'), 'first');
    const duplicate = provider.sendMessage('thread-A', input('Same'), 'first');
    await provider.start('turn-A');
    await Promise.all([first, duplicate]);
    const next = await provider.sendMessage('thread-A', input('Same'), 'second');
    expect(next.id).toBe('second');
    expect(provider.messages('thread-A')).toHaveLength(2);
    expect(await provider.requests()).toHaveLength(1);
    await expect(provider.sendMessage('thread-A', input('Different'), 'first')).rejects.toThrow('different submission');
    expect(provider.removeQueuedMessage('thread-A', 'second')).toBe(true);
    await provider.complete('turn-A');
    expect(await provider.requests()).toHaveLength(1);
  });

  it('does not change a queued message when the caller edits its input array', async () => {
    const provider = await setup();
    await active(provider);
    const queued = input('Original');
    await provider.sendMessage('thread-A', queued, 'message-B');
    const text = queued[0];
    if (text?.type === 'text') text.text = 'Changed later';
    await provider.complete('turn-A');
    expect((await provider.requests())[1]?.input).toEqual(input('Original'));
    await provider.start('turn-B');
  });

  it('preserves a rejected steering submission for normal queue delivery', async () => {
    const provider = await setup();
    await active(provider);
    const queued = await provider.sendMessage('thread-A', input('B'), 'message-B');
    const failed = expect(provider.steerTurn('thread-A', queued.id)).rejects.toThrow('Turn ended');
    await provider.steer('turn-A', 'Turn ended', -32602);
    await failed;
    expect(provider.messages('thread-A')[1]?.status).toBe('queued');
    await provider.complete('turn-A');
    await provider.start('turn-B');
    expect((await provider.requests())[1]?.input).toEqual(input('B'));
  });

  it('shares repeated steering requests and waits for their result before advancing', async () => {
    const provider = await setup();
    await active(provider);
    await provider.sendMessage('thread-A', input('B'), 'message-B');
    await provider.sendMessage('thread-A', input('C'), 'message-C');
    const first = provider.steerTurn('thread-A', 'message-B');
    const second = provider.steerTurn('thread-A', 'message-B');
    await provider.complete('turn-A');
    expect(await provider.requests('turn/steer')).toHaveLength(1);
    expect(await provider.requests()).toHaveLength(1);
    await provider.steer('turn-A');
    await Promise.all([first, second]);
    await provider.start('turn-C');
    expect(provider.messages('thread-A')[1]).toMatchObject({ status: 'accepted', turnId: 'turn-A' });
    expect((await provider.requests())[1]?.input).toEqual(input('C'));
  });

  it('keeps steering input queued when the turn is still starting', async () => {
    const provider = await setup();
    const sent = provider.sendMessage('thread-A', input('A'));
    const queued = await provider.sendMessage('thread-A', input('B'));
    expect((await provider.steerTurn('thread-A', queued.id)).status).toBe('queued');
    expect(await provider.requests('turn/steer')).toEqual([]);
    await provider.start('turn-A');
    await sent;
    await provider.complete('turn-A');
    await provider.start('turn-B');
  });

  it('pauses follow-ups after a failed turn until the caller explicitly resumes', async () => {
    const provider = await setup();
    await active(provider);
    await provider.sendMessage('thread-A', input('Create the PR'), 'message-B');
    await provider.complete('turn-A', 'thread-A', 'failed');
    expect(await provider.requests()).toHaveLength(1);
    await expect(provider.waitUntilIdle('thread-A')).rejects.toThrow('failed');
    const resumed = provider.resumeQueue('thread-A');
    await provider.start('turn-B');
    await resumed;
    expect((await provider.requests())[1]?.input).toEqual(input('Create the PR'));
  });

  it('cleans up a malformed start response instead of keeping a starting turn forever', async () => {
    const provider = await setup();
    const failed = expect(provider.sendMessage('thread-A', input('A'), 'message-A')).rejects.toThrow();
    await provider.settleStart();
    await failed;
    await expect(provider.waitUntilIdle('thread-A')).rejects.toThrow();
    expect(provider.messages('thread-A')[0]?.status).toBe('unknown');
    expect((await provider.sendMessage('thread-A', input('B'))).status).toBe('queued');
  });

  it('settles a start and idle waiters when saving accepted delivery fails', async () => {
    const provider = await setup();
    const update = provider.store.updateCodexMessage.bind(provider.store);
    vi.spyOn(provider.store, 'updateCodexMessage').mockImplementation((id, status, turnId, error) => {
      if (status === 'accepted') throw new Error('Database write failed');
      return update(id, status, turnId, error);
    });
    const failed = expect(provider.sendMessage('thread-A', input('A'), 'message-A')).rejects.toThrow(
      'Database write failed',
    );
    const idle = provider.waitUntilIdle('thread-A').catch((error: unknown) => error);
    await provider.start('turn-A');
    await failed;
    await provider.close();
    expect(await idle).toMatchObject({ message: 'Database write failed' });
    expect(provider.messages('thread-A')[0]).toMatchObject({ id: 'message-A', status: 'unknown' });
  });

  it('closes when uncertain start delivery cannot be saved', async () => {
    const provider = await setup();
    const update = provider.store.updateCodexMessage.bind(provider.store);
    vi.spyOn(provider.store, 'updateCodexMessage').mockImplementation((id, status, turnId, error) => {
      if (status === 'unknown') throw new Error('Database write failed');
      return update(id, status, turnId, error);
    });
    vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const failed = expect(provider.sendMessage('thread-A', input('A'), 'message-A')).rejects.toThrow(
      'Database write failed',
    );
    const idle = provider.waitUntilIdle('thread-A').catch((error: unknown) => error);
    await provider.failStart();
    await failed;
    await provider.close();
    expect(await idle).toMatchObject({ message: 'Database write failed' });
    expect(provider.messages('thread-A')[0]).toMatchObject({ id: 'message-A', status: 'submitting' });
  });

  it('closes when accepted steering delivery cannot be saved', async () => {
    const provider = await setup();
    await active(provider);
    await provider.sendMessage('thread-A', input('B'), 'message-B');
    const update = provider.store.updateCodexMessage.bind(provider.store);
    vi.spyOn(provider.store, 'updateCodexMessage').mockImplementation((id, status, turnId, error) => {
      if (id === 'message-B' && status === 'accepted') throw new Error('Database write failed');
      return update(id, status, turnId, error);
    });
    const failed = expect(provider.steerTurn('thread-A', 'message-B')).rejects.toThrow('Database write failed');
    const idle = provider.waitUntilIdle('thread-A').catch((error: unknown) => error);
    await provider.steer('turn-A');
    await failed;
    await provider.close();
    expect(await idle).toMatchObject({ message: 'Database write failed' });
    expect(provider.messages('thread-A')[1]).toMatchObject({ id: 'message-B', status: 'unknown' });
  });

  it('rejects unknown threads and invalid empty input before submitting', async () => {
    const provider = await setup();
    await expect(provider.sendMessage('unknown', input('A'))).rejects.toThrow('Start or resume');
    expect(() => provider.setThreadPermissions('unknown', 'fullAccess')).toThrow('Start or resume');
    await expect(provider.sendMessage('thread-A', [])).rejects.toThrow();
    expect(await provider.requests()).toEqual([]);
  });
});

describe('Codex lifecycle', () => {
  it('settles immediately when already idle, including a terminal start response', async () => {
    const provider = await setup();
    await provider.waitUntilIdle('thread-A');
    const sent = provider.sendMessage('thread-A', input('A'));
    await provider.start('turn-A', 'thread-A', 'completed');
    await sent;
    await provider.waitUntilIdle('thread-A');
  });

  it('rejects idle waiters on process exit and retains pending work', async () => {
    const provider = await setup();
    await active(provider);
    await provider.sendMessage('thread-A', input('B'), 'message-B');
    const idle = expect(provider.waitUntilIdle('thread-A')).rejects.toThrow('exited');
    await provider.crash();
    await idle;
    await expect(provider.sendMessage('thread-A', input('C'))).rejects.toThrow('closed');
    expect(provider.messages('thread-A')[1]).toMatchObject({ id: 'message-B', status: 'queued' });
  });

  it('settles every thread on process exit even when recovery state cannot be saved', async () => {
    const provider = await setup();
    await active(provider);
    const other = provider.sendMessage('thread-B', input('Other'));
    await provider.start('turn-B', 'thread-B');
    await other;
    vi.spyOn(provider.store, 'saveCodexThread').mockImplementation(() => {
      throw new Error('Database is read-only');
    });
    vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const idleA = provider.waitUntilIdle('thread-A').catch((error: unknown) => error);
    const idleB = provider.waitUntilIdle('thread-B').catch((error: unknown) => error);
    await provider.crash();
    expect(await idleA).toMatchObject({ message: 'Codex exited' });
    expect(await idleB).toMatchObject({ message: 'Codex exited' });
  });

  it('reports turn completion and settles waiters when saving a failed outcome fails', async () => {
    const provider = await setup();
    await active(provider);
    await provider.sendMessage('thread-A', input('Dependent work'), 'message-B');
    vi.spyOn(provider.store, 'saveCodexThread').mockImplementation(() => {
      throw new Error('Database is read-only');
    });
    vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const idle = provider.waitUntilIdle('thread-A').catch((error: unknown) => error);
    await provider.complete('turn-A', 'thread-A', 'failed');
    expect(provider.events.some((event) => event.kind === 'turn.completed' && event.turn.status === 'failed')).toBe(
      true,
    );
    await provider.close();
    expect(await idle).toMatchObject({ message: 'Database is read-only' });
    expect(provider.messages('thread-A')[1]?.status).toBe('queued');
  });

  it('rejects waiters on thread closure while leaving another thread usable', async () => {
    const provider = await setup();
    await active(provider);
    const idle = expect(provider.waitUntilIdle('thread-A')).rejects.toThrow('thread closed');
    await provider.emit('thread/closed', { threadId: 'thread-A' });
    await idle;
    await expect(provider.sendMessage('thread-A', input('C'))).rejects.toThrow('thread closed');
    const sent = provider.sendMessage('thread-B', input('Other'));
    await provider.start('turn-B', 'thread-B');
    await sent;
  });

  it('retains message records after provider shutdown and reopening the database', async () => {
    const provider = await setup();
    await active(provider);
    await provider.sendMessage('thread-A', input('B'), 'message-B');
    await provider.close();
    const { Store } = await import('../src/store');
    const reopened = new Store(provider.home);
    try {
      expect(reopened.codexMessages('thread-A')).toMatchObject([
        { id: 'message-A', status: 'accepted' },
        { id: 'message-B', status: 'queued' },
      ]);
    } finally {
      reopened.close();
    }
    await provider.close();
  });

  it('marks a start interrupted by process exit as uncertain', async () => {
    const provider = await setup();
    const failed = expect(provider.sendMessage('thread-A', input('A'), 'message-A')).rejects.toThrow();
    await provider.flush();
    await provider.crash();
    await failed;
    expect(provider.messages('thread-A')[0]?.status).toBe('unknown');
  });

  it('rejects a running-turn permission change and applies it after completion', async () => {
    const provider = await setup();
    provider.setThreadPermissions('thread-A', 'fullAccess');
    await active(provider);
    expect(() => provider.setThreadPermissions('thread-A', 'askMe')).toThrow('Stop the turn');
    await provider.complete('turn-A');
    provider.setThreadPermissions('thread-A', 'askMe');
    const sent = provider.sendMessage('thread-A', input('B'));
    await provider.start('turn-B');
    await sent;
    expect((await provider.requests())[1]?.sandboxPolicy?.type).toBe('workspaceWrite');
  });

  it('interrupts a timed-out turn and keeps dependent work paused', async () => {
    const provider = await setup({ turnTimeoutMs: 50 });
    vi.useFakeTimers();
    await active(provider);
    await provider.sendMessage('thread-A', input('B'));
    const idle = expect(provider.waitUntilIdle('thread-A')).rejects.toThrow('timed out');
    await vi.advanceTimersByTimeAsync(50);
    expect(await provider.requests('turn/interrupt')).toHaveLength(1);
    await provider.interrupt();
    await provider.complete('turn-A');
    await idle;
    expect(await provider.requests()).toHaveLength(1);
  });

  it('does not time out a turn waiting for user input', async () => {
    const provider = await setup({ turnTimeoutMs: 50 });
    vi.useFakeTimers();
    await active(provider);
    await provider.ask('item/tool/requestUserInput', 'question', {
      threadId: 'thread-A',
      turnId: 'turn-A',
      itemId: 'question-item',
      questions: [],
      isBlocking: true,
      autoResolutionMs: null,
    });
    await vi.advanceTimersByTimeAsync(100);
    expect(await provider.requests('turn/interrupt')).toEqual([]);
    expect(provider.answerUserInput('question', { answers: {} })).toBe(true);
    await provider.complete('turn-A');
  });

  it('still enforces the turn deadline when a question does not block execution', async () => {
    const provider = await setup({ turnTimeoutMs: 50 });
    vi.useFakeTimers();
    await active(provider);
    await provider.ask('item/tool/requestUserInput', 'question', {
      threadId: 'thread-A',
      turnId: 'turn-A',
      itemId: 'question-item',
      questions: [],
      isBlocking: false,
      autoResolutionMs: null,
    });
    const idle = expect(provider.waitUntilIdle('thread-A')).rejects.toThrow('timed out');
    await vi.advanceTimersByTimeAsync(50);
    expect(await provider.requests('turn/interrupt')).toHaveLength(1);
    await provider.interrupt();
    await provider.complete('turn-A');
    await idle;
    expect(provider.userInputsWaiting('thread-A')).toEqual([]);
  });

  it('does not close the next turn after a late timed-out interrupt response', async () => {
    const provider = await setup({ turnTimeoutMs: 10000 });
    vi.useFakeTimers();
    await active(provider);
    await provider.sendMessage('thread-A', input('B'), 'message-B');
    await vi.advanceTimersByTimeAsync(10000);
    await provider.complete('turn-A');
    const resumed = provider.resumeQueue('thread-A');
    await provider.start('turn-B');
    await resumed;
    await provider.interrupt();
    await vi.advanceTimersByTimeAsync(5000);
    expect((await provider.sendMessage('thread-A', input('C'))).status).toBe('queued');
    await provider.complete('turn-B');
    await provider.start('turn-C');
  });

  it('closes a process that acknowledges Stop without completing the turn', async () => {
    const provider = await setup({ turnTimeoutMs: 10000 });
    vi.useFakeTimers();
    await active(provider);
    const idle = expect(provider.waitUntilIdle('thread-A')).rejects.toThrow('provider closed');
    const stopped = provider.stopTurn('thread-A');
    await provider.interrupt();
    await stopped;
    await vi.advanceTimersByTimeAsync(5000);
    await idle;
    await expect(provider.sendMessage('thread-A', input('B'))).rejects.toThrow('closed');
  });
});

describe('Codex restart recovery', () => {
  it('shares concurrent resumes without losing the restored turn state', async () => {
    const previous = await setup();
    previous.store.addCodexMessage('retained', 'thread-A', input('Retained work'));
    const next = await previous.restart();
    await Promise.all([next.provider.resumeThread('thread-A'), next.provider.resumeThread('thread-A')]);
    let settled = false;
    const idle = next.provider.waitUntilIdle('thread-A').then(
      () => {
        settled = true;
      },
      (error: unknown) => error,
    );
    await next.start('turn-resumed');
    expect(settled).toBe(false);
    await next.control('test/emit', {
      method: 'turn/completed',
      params: { threadId: 'thread-A', turn: turn('turn-resumed', 'completed') },
    });
    expect(await idle).toBeUndefined();
    expect(settled).toBe(true);
  });

  it('continues retained queued work after a clean completed turn', async () => {
    const previous = await setup();
    previous.store.addCodexMessage('retained', 'thread-A', input('Retained work'));
    const next = await previous.restart();
    await next.provider.resumeThread('thread-A');
    await next.start('turn-resumed');
    expect(next.provider.messages('thread-A')).toMatchObject([
      { id: 'retained', status: 'accepted', turnId: 'turn-resumed' },
    ]);
  });

  it('reconciles accepted input whose start reply was lost without submitting it again', async () => {
    const previous = await setup();
    previous.store.addCodexMessage('lost-reply', 'thread-A', input('Already executed'));
    previous.store.updateCodexMessage('lost-reply', 'submitting');
    const next = await previous.restart();
    await next.control('test/setThread', {
      thread: {
        id: 'thread-A',
        cwd: 'C:\\project',
        model: 'fake-model',
        status: { type: 'idle' },
        turns: [
          {
            ...turn('completed-turn', 'completed'),
            items: [
              { type: 'userMessage', id: 'native-message', clientId: 'lost-reply', content: input('Already executed') },
            ],
          },
        ],
      },
    });
    await next.provider.resumeThread('thread-A');
    expect(next.provider.messages('thread-A')[0]).toMatchObject({ status: 'accepted', turnId: 'completed-turn' });
    expect(await next.control('test/turnRequests', {})).toEqual([]);
  });

  it('does not interpret unfinished saved history as a live turn or automatically repeat uncertain work', async () => {
    const previous = await setup();
    previous.store.addCodexMessage('uncertain', 'thread-A', input('Uncertain work'));
    previous.store.updateCodexMessage('uncertain', 'submitting');
    const next = await previous.restart();
    await next.control('test/setThread', {
      thread: {
        id: 'thread-A',
        cwd: 'C:\\project',
        model: 'fake-model',
        status: { type: 'idle' },
        turns: [turn('old-turn')],
      },
    });
    await next.provider.resumeThread('thread-A');
    await expect(next.provider.waitUntilIdle('thread-A')).rejects.toThrow('interrupted');
    expect(next.provider.messages('thread-A')[0]?.status).toBe('unknown');
    expect(await next.control('test/turnRequests', {})).toEqual([]);
  });
});

describe('Codex discovery and user requests', () => {
  it('cannot approve a request after provider shutdown has begun', async () => {
    const provider = await setup();
    await provider.ask('item/fileChange/requestApproval', 'file', {
      threadId: 'thread-A',
      turnId: 'turn-A',
      itemId: 'file-item',
      startedAtMs: 100,
    });
    const closing = provider.close();
    expect(provider.decideFile('file', 'accept')).toBe(false);
    await closing;
    expect(provider.approvalsWaiting('thread-A')).toEqual([]);
    expect(provider.events.filter((event) => event.kind === 'approvals.changed').at(-1)).toMatchObject({
      approvals: [],
    });
  });

  it('emits approval snapshots as requests arrive, file details change and requests resolve', async () => {
    const provider = await setup();
    await provider.ask('item/commandExecution/requestApproval', 'command', {
      kind: 'command',
      threadId: 'thread-A',
      turnId: 'turn-A',
      itemId: 'command-item',
      startedAtMs: 100,
      environmentId: null,
      command: 'npm test',
    });
    await provider.ask('item/fileChange/requestApproval', 'file', {
      threadId: 'thread-A',
      turnId: 'turn-A',
      itemId: 'file-item',
      startedAtMs: 100,
    });
    expect(provider.events.filter((event) => event.kind === 'approvals.changed')).toMatchObject([
      { threadId: 'thread-A', approvals: [{ requestId: 'command', kind: 'command' }] },
      { threadId: 'thread-A', approvals: [{ requestId: 'command' }, { requestId: 'file', changes: null }] },
    ]);
    await provider.emit('item/started', {
      threadId: 'thread-A',
      turnId: 'turn-A',
      startedAtMs: 100,
      item: {
        type: 'fileChange',
        id: 'file-item',
        status: 'inProgress',
        changes: [{ path: 'new.ts', kind: { type: 'add' }, diff: '+new' }],
      },
    });
    expect(provider.events.filter((event) => event.kind === 'approvals.changed').at(-1)).toMatchObject({
      approvals: [{ requestId: 'command' }, { requestId: 'file', changes: [{ path: 'new.ts', diff: '+new' }] }],
    });
    expect(provider.decideCommand('command', 'decline')).toBe(true);
    await provider.emit('serverRequest/resolved', { threadId: 'thread-A', requestId: 'file' });
    expect(provider.events.filter((event) => event.kind === 'approvals.changed').at(-1)).toMatchObject({
      approvals: [],
    });
    expect(provider.fileWaiting('file')).toBeUndefined();
    expect(provider.events.at(-1)).toMatchObject({ kind: 'serverRequest.resolved', requestId: 'file' });
  });

  it('loads every model page and keeps models with matching display names', async () => {
    const provider = await setup();
    await provider.control('test/models', {
      pages: [
        { data: [{ model: 'one', displayName: 'Same name', description: 'First' }], nextCursor: '1' },
        { data: [{ model: 'two', displayName: 'Same name', description: 'Second' }], nextCursor: null },
      ],
    });
    expect([...(await provider.providerModels())].map(([id]) => id)).toEqual(['one', 'two']);
    expect([...(await provider.findSkills('C:\\chosen'))][0]?.[0]).toBe('C:\\chosen/SKILL.md');
    expect([...(await provider.findPlugins('C:\\chosen'))][0]?.[1].marketplace).toBe('C:\\chosen');
  });

  it('rejects a repeating model cursor instead of looping forever', async () => {
    const provider = await setup();
    await provider.control('test/models', {
      pages: [
        { data: [], nextCursor: '1' },
        { data: [], nextCursor: '1' },
      ],
    });
    await expect(provider.providerModels()).rejects.toThrow('repeated a model page');
  });

  it('answers structured questions by their request ID and suppresses stale answers', async () => {
    const provider = await setup();
    await provider.ask('item/tool/requestUserInput', 'question', {
      threadId: 'thread-A',
      turnId: 'turn-A',
      itemId: 'item-A',
      questions: [],
      isBlocking: true,
      autoResolutionMs: null,
    });
    const answer = provider.answers();
    expect(provider.answerUserInput('question', { answers: { choice: { answers: ['Yes'] } } })).toBe(true);
    expect(await answer).toMatchObject({ id: 'question', result: { answers: { choice: { answers: ['Yes'] } } } });
    expect(provider.answerUserInput('question', { answers: {} })).toBe(false);
    await provider.ask('item/tool/requestUserInput', 'stale', {
      threadId: 'thread-A',
      turnId: 'turn-A',
      itemId: 'item-B',
      questions: [],
      isBlocking: true,
      autoResolutionMs: null,
    });
    await provider.emit('serverRequest/resolved', { threadId: 'thread-A', requestId: 'stale' });
    expect(provider.answerUserInput('stale', { answers: {} })).toBe(false);
  });

  it('can deny permission requests and cancel MCP elicitation without unsupported-method errors', async () => {
    const provider = await setup();
    await provider.ask('item/permissions/requestApproval', 'permission', {
      threadId: 'thread-A',
      turnId: 'turn-A',
      itemId: 'permission-A',
      environmentId: null,
      startedAtMs: 100,
      cwd: 'C:\\project',
      reason: null,
      permissions: { network: null, fileSystem: null },
    });
    const permissionAnswer = provider.answers();
    expect(provider.decidePermissions('permission', { permissions: {}, scope: 'turn' })).toBe(true);
    expect(await permissionAnswer).toMatchObject({ result: { permissions: {}, scope: 'turn' } });
    await provider.ask('mcpServer/elicitation/request', 'elicitation', {
      threadId: 'thread-A',
      turnId: null,
      serverName: 'example',
      mode: 'url',
      message: 'Sign in',
      url: 'https://example.com',
      elicitationId: 'flow',
      _meta: null,
    });
    const elicitationAnswer = provider.answers();
    expect(provider.answerElicitation('elicitation', { action: 'cancel', content: null, _meta: null })).toBe(true);
    expect(await elicitationAnswer).toMatchObject({ result: { action: 'cancel', content: null } });
  });
});
