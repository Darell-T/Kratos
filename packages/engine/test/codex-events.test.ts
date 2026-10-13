import { describe, expect, it, vi } from 'vitest';
import type { v2 } from '@kratos/codex';
import type { CodexEvent } from '../src/providers/codex';
import { setup, input, turn } from './codex-fixture';

describe('Codex structured output through the client transport', () => {
  it('keeps interleaved text attributed to its thread, turn and item without printing it', async () => {
    const provider = await setup();
    const write = vi.spyOn(process.stdout, 'write');
    const log = vi.spyOn(console, 'log');
    await provider.emit('item/agentMessage/delta', {
      threadId: 'thread-A',
      turnId: 'turn-A',
      itemId: 'message-A',
      delta: 'Hello',
    });
    await provider.emit('item/agentMessage/delta', {
      threadId: 'thread-B',
      turnId: 'turn-B',
      itemId: 'message-B',
      delta: 'Another reply',
    });
    await provider.emit('item/agentMessage/delta', {
      threadId: 'thread-A',
      turnId: 'turn-A',
      itemId: 'message-A',
      delta: ' world',
    });

    expect(provider.events.filter((event) => event.kind !== 'message.updated')).toEqual([
      { kind: 'message.delta', threadId: 'thread-A', turnId: 'turn-A', itemId: 'message-A', delta: 'Hello' },
      { kind: 'message.delta', threadId: 'thread-B', turnId: 'turn-B', itemId: 'message-B', delta: 'Another reply' },
      { kind: 'message.delta', threadId: 'thread-A', turnId: 'turn-A', itemId: 'message-A', delta: ' world' },
    ]);
    expect(write).not.toHaveBeenCalled();
    expect(log).not.toHaveBeenCalled();
  });

  it('preserves final assistant text and tool results in completed items', async () => {
    const provider = await setup();
    await provider.emit('item/completed', {
      threadId: 'thread-A',
      turnId: 'turn-A',
      completedAtMs: 102000,
      item: {
        type: 'agentMessage',
        id: 'message-A',
        text: 'The complete answer',
        phase: 'final_answer',
        memoryCitation: null,
        delivery: null,
        questions: null,
      },
    });
    await provider.emit('item/completed', {
      threadId: 'thread-B',
      turnId: 'turn-B',
      completedAtMs: 103000,
      item: {
        type: 'commandExecution',
        id: 'command-B',
        pluginId: null,
        scriptPath: null,
        command: 'npm test',
        cwd: 'C:\\project',
        processId: null,
        source: 'agent',
        status: 'failed',
        commandActions: [],
        aggregatedOutput: 'One test failed',
        exitCode: 1,
        durationMs: 1200,
      },
    });

    expect(provider.events.filter((event) => event.kind !== 'message.updated')).toMatchObject([
      {
        kind: 'item.completed',
        threadId: 'thread-A',
        turnId: 'turn-A',
        completedAtMs: 102000,
        item: { type: 'agentMessage', id: 'message-A', text: 'The complete answer', phase: 'final_answer' },
      },
      {
        kind: 'item.completed',
        threadId: 'thread-B',
        turnId: 'turn-B',
        item: { type: 'commandExecution', status: 'failed', aggregatedOutput: 'One test failed', exitCode: 1 },
      },
    ]);
  });

  it('reports activity with the original file changes', async () => {
    const provider = await setup();
    await provider.emit('item/started', {
      threadId: 'thread-A',
      turnId: 'turn-A',
      startedAtMs: 100000,
      item: {
        type: 'fileChange',
        id: 'file-A',
        status: 'inProgress',
        changes: [{ path: 'src/app.ts', kind: { type: 'update', move_path: null }, diff: '-old\n+new' }],
      },
    });

    expect(provider.events.filter((event) => event.kind !== 'message.updated')).toMatchObject([
      {
        kind: 'item.started',
        threadId: 'thread-A',
        turnId: 'turn-A',
        item: { id: 'file-A', changes: [{ path: 'src/app.ts', diff: '-old\n+new' }] },
      },
    ]);
  });

  it.each(['completed', 'failed', 'interrupted'] as const)('preserves the %s turn outcome', async (status) => {
    const provider = await setup();
    const error: v2.TurnError | null =
      status === 'failed'
        ? {
            message: 'Usage limit reached',
            codexErrorInfo: 'usageLimitExceeded',
            additionalDetails: null,
            misalignment: null,
          }
        : null;
    await provider.emit('turn/started', { threadId: 'thread-A', turn: turn('turn-A') });
    await provider.emit('turn/completed', { threadId: 'thread-A', turn: turn('turn-A', status, error) });

    expect(provider.events.filter((event) => event.kind !== 'message.updated')).toMatchObject([
      { kind: 'turn.started', threadId: 'thread-A', turn: { id: 'turn-A', status: 'inProgress' } },
      { kind: 'turn.completed', threadId: 'thread-A', turn: { id: 'turn-A', status, error, durationMs: 2000 } },
    ]);
  });

  it('unsubscribes one listener without stopping another', async () => {
    const provider = await setup();
    const received: CodexEvent[] = [];
    const unsubscribe = provider.onEvent((event) => received.push(event));
    await provider.emit('item/agentMessage/delta', {
      threadId: 'thread-A',
      turnId: 'turn-A',
      itemId: 'message-A',
      delta: 'First',
    });
    unsubscribe();
    await provider.emit('item/agentMessage/delta', {
      threadId: 'thread-A',
      turnId: 'turn-A',
      itemId: 'message-A',
      delta: 'Second',
    });

    expect(received.filter((event) => event.kind !== 'message.updated')).toMatchObject([{ delta: 'First' }]);
    expect(provider.events.filter((event) => event.kind !== 'message.updated')).toMatchObject([
      { delta: 'First' },
      { delta: 'Second' },
    ]);
  });

  it('keeps delivery and queued work running when a listener throws', async () => {
    const provider = await setup();
    vi.spyOn(console, 'error').mockImplementation(() => undefined);
    provider.onEvent(() => {
      throw new Error('Listener failed');
    });
    const received: CodexEvent[] = [];
    provider.onEvent((event) => received.push(event));
    const started = provider.sendMessage('thread-A', input('A'));
    await provider.settleStart({ turn: turn('turn-A') });
    await started;
    await provider.sendMessage('thread-A', input('B'));
    await provider.emit('turn/completed', { threadId: 'thread-A', turn: turn('turn-A', 'completed') });

    expect(received.filter((event) => event.kind !== 'message.updated')).toMatchObject([
      { kind: 'turn.completed', threadId: 'thread-A', turn: { id: 'turn-A' } },
    ]);
    expect(await provider.requests()).toMatchObject([{ input: input('A') }, { input: input('B') }]);
    await provider.settleStart({ turn: turn('turn-B') });
  });

  it('emits a start failure while still rejecting the caller', async () => {
    const provider = await setup();
    const sent = expect(provider.sendMessage('thread-A', input('A'))).rejects.toThrow('Start rejected');
    await provider.settleStart(undefined, 'Start rejected');
    await sent;

    expect(provider.events.filter((event) => event.kind !== 'message.updated')).toEqual([
      { kind: 'turn.startFailed', threadId: 'thread-A', message: 'Start rejected' },
    ]);
  });

  it('exposes a background queued start failure through events', async () => {
    const provider = await setup();
    const error = vi.spyOn(console, 'error');
    const started = provider.sendMessage('thread-A', input('A'));
    await provider.settleStart({ turn: turn('turn-A') });
    await started;
    await provider.sendMessage('thread-A', input('B'));
    await provider.emit('turn/completed', { threadId: 'thread-A', turn: turn('turn-A', 'completed') });
    await provider.settleStart(undefined, 'Queued start rejected');
    await provider.flush();

    expect(provider.events.filter((event) => event.kind !== 'message.updated')).toMatchObject([
      { kind: 'turn.completed', turn: { id: 'turn-A' } },
      { kind: 'turn.startFailed', threadId: 'thread-A', message: 'Queued start rejected' },
    ]);
    expect(error).not.toHaveBeenCalled();
  });

  it('preserves native error details and reports process exit', async () => {
    const provider = await setup();
    await provider.emit('error', {
      threadId: 'thread-A',
      turnId: 'turn-A',
      willRetry: true,
      error: {
        message: 'Rate limited',
        codexErrorInfo: 'rateLimitExceeded',
        additionalDetails: 'Try again',
        misalignment: null,
      },
    });
    await provider.crash();

    expect(provider.events.filter((event) => event.kind !== 'message.updated')).toMatchObject([
      {
        kind: 'error',
        threadId: 'thread-A',
        turnId: 'turn-A',
        willRetry: true,
        error: { message: 'Rate limited', codexErrorInfo: 'rateLimitExceeded' },
      },
      { kind: 'provider.exited', exitCode: 3 },
    ]);
  });
});
