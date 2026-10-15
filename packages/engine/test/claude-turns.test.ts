import { describe, expect, it, vi } from 'vitest';
import { A, B, C, D, E, flush, folder, input, setup } from './claude-fixture';

const interrupted = {
  subtype: 'error_during_execution',
  is_error: true,
  errors: ['Interrupted by user'],
  terminal_reason: 'aborted_streaming',
};

describe('Claude turns through a scripted SDK', () => {
  it('launches Claude lazily and delivers queued messages one turn at a time, in order', async () => {
    const claude = await setup();
    expect(claude.launches).toEqual([]);

    let accepted = false;
    const sent = claude.sendMessage(claude.thread, input('Fix the test'), A).then((message) => {
      accepted = true;
      return message;
    });
    await flush();
    const { options } = claude.launch();
    expect(options).toMatchObject({
      cwd: folder,
      sessionId: claude.thread,
      permissionMode: 'default',
      settingSources: ['user', 'project', 'local'],
      persistSession: true,
      includePartialMessages: true,
      extraArgs: { 'replay-user-messages': null },
    });
    expect(Object.keys(options)).not.toContain('resume');
    expect(Object.keys(options)).not.toContain('model');
    expect(Object.keys(options)).not.toContain('allowDangerouslySkipPermissions');
    expect(claude.launch().written).toEqual([
      { type: 'user', message: { role: 'user', content: input('Fix the test') }, parent_tool_use_id: null, uuid: A },
    ]);
    expect(claude.store.getClaudeThread(claude.thread)?.runningTurn).toBe(A);
    expect(accepted).toBe(false);

    await claude.init('claude-resolved');
    await claude.accept(A);
    expect(await sent).toMatchObject({ id: A, status: 'accepted', turnId: A });
    expect(await claude.sendMessage(claude.thread, input('Update the docs'), B)).toMatchObject({ status: 'queued' });
    await claude.sendMessage(claude.thread, input('Open a PR'), C);
    let idle = false;
    const waiting = claude.waitUntilIdle(claude.thread).then(() => {
      idle = true;
    });
    expect(claude.written()).toEqual([A]);

    await claude.finish([A]);
    expect(claude.launch().written[1]).toMatchObject({ uuid: B, message: { content: input('Update the docs') } });
    await claude.accept(B);
    await claude.finish([B], interrupted);
    expect(claude.written()).toEqual([A, B, C]);
    await claude.accept(C);
    expect(idle).toBe(false);
    await claude.finish([C]);
    await waiting;

    expect(claude.launches).toHaveLength(1);
    expect(claude.store.getClaudeThread(claude.thread)).toMatchObject({ model: 'claude-resolved', runningTurn: null });
    expect(
      claude.events.flatMap((event) =>
        event.kind === 'turn.started'
          ? [['started', event.messageId]]
          : event.kind === 'turn.completed'
            ? [['completed', event.outcome, ...event.messageIds]]
            : [],
      ),
    ).toEqual([
      ['started', A],
      ['completed', 'completed', A],
      ['started', B],
      ['completed', 'interrupted', B],
      ['started', C],
      ['completed', 'completed', C],
    ]);
  });

  it('keeps queued messages until the stopped turn reports its result, and keeps the deadline after a failed Stop', async () => {
    const claude = await setup({ turnTimeoutMs: 10000 });
    vi.useFakeTimers();
    await claude.start(A);
    await claude.sendMessage(claude.thread, input('B'), B);

    const stopped = claude.stopTurn(claude.thread);
    const stoppedAgain = claude.stopTurn(claude.thread);
    await Promise.all([stopped, stoppedAgain]);
    await flush();
    expect(claude.launch().interrupts).toBe(1);
    expect(claude.written()).toEqual([A]);

    await claude.finish([A], interrupted);
    expect(claude.events.filter((event) => event.kind === 'turn.completed')).toMatchObject([
      { turnId: A, messageIds: [A], outcome: 'interrupted', error: 'Interrupted by user' },
    ]);
    expect(claude.written()).toEqual([A, B]);
    await claude.accept(B);

    claude.failInterrupts(new Error('Interrupt rejected'));
    await expect(claude.stopTurn(claude.thread)).rejects.toThrow('Interrupt rejected');
    claude.failInterrupts();
    await vi.advanceTimersByTimeAsync(10000);
    const message = 'Claude turn timed out after 10000 ms';
    expect(claude.launch().closed).toBe(false);
    expect(claude.events).toContainEqual({ kind: 'turn.timedOut', threadId: claude.thread, turnId: B, message });
    expect(claude.launch().interrupts).toBe(3);
    await claude.finish([B], interrupted);
    await expect(claude.waitUntilIdle(claude.thread)).rejects.toThrow(message);
    expect(claude.messages(claude.thread)).toMatchObject([
      { id: A, status: 'accepted' },
      { id: B, status: 'accepted' },
    ]);
  });

  it('closes a session that never acknowledges a Stop, then resumes that session for the next message', async () => {
    const claude = await setup();
    vi.useFakeTimers();
    await claude.start(A);
    await claude.sendMessage(claude.thread, input('B'), B);
    const idle = claude.waitUntilIdle(claude.thread).catch((error: unknown) => error);

    claude.failInterrupts('hang');
    const stopped = claude.stopTurn(claude.thread);
    await vi.advanceTimersByTimeAsync(5000);
    await flush();
    const message = 'Claude did not stop the turn; its process was closed';
    expect(claude.launch().closed).toBe(true);
    await expect(stopped).resolves.toBeUndefined();
    expect(await idle).toMatchObject({ message });
    expect(claude.events).toContainEqual({ kind: 'session.exited', threadId: claude.thread, message });
    expect(claude.messages(claude.thread)).toMatchObject([
      { id: A, status: 'accepted' },
      { id: B, status: 'queued' },
    ]);
    expect(claude.store.getClaudeThread(claude.thread)).toMatchObject({ pausedError: message, runningTurn: null });

    const resumed = claude.resumeQueue(claude.thread);
    await flush();
    expect(claude.launches).toHaveLength(2);
    expect(claude.launch().options).toMatchObject({ cwd: folder, resume: claude.thread });
    expect(Object.keys(claude.launch().options)).not.toContain('sessionId');
    expect(claude.written()).toEqual([B]);
    await claude.accept(B);
    await resumed;
  });

  it('pauses follow-up messages after a failed result, even one that raced a Stop, until the queue is resumed', async () => {
    const claude = await setup();
    await claude.start(A);
    await claude.sendMessage(claude.thread, input('B'), B);
    await claude.sendMessage(claude.thread, input('C'), C);
    const idle = claude.waitUntilIdle(claude.thread).catch((error: unknown) => error);

    await claude.stopTurn(claude.thread);
    await claude.finish([A], { is_error: true, result: 'API Error: 529 overloaded', terminal_reason: 'api_error' });
    expect(claude.events.filter((event) => event.kind === 'turn.completed').at(-1)).toMatchObject({
      messageIds: [A],
      outcome: 'failed',
      error: 'API Error: 529 overloaded',
    });
    expect(claude.written()).toEqual([A]);
    expect(await idle).toMatchObject({ message: 'API Error: 529 overloaded' });

    const resumed = claude.resumeQueue(claude.thread);
    await flush();
    await claude.accept(B);
    await resumed;
    await claude.finish([B], { subtype: 'error_max_turns', errors: ['Reached max turns', 'Stopped early'] });
    expect(claude.events.filter((event) => event.kind === 'turn.completed').at(-1)).toMatchObject({
      messageIds: [B],
      outcome: 'failed',
      error: 'Reached max turns\nStopped early',
    });
    await expect(claude.waitUntilIdle(claude.thread)).rejects.toThrow('Reached max turns');
    expect(claude.written()).toEqual([A, B]);
    expect(claude.messages(claude.thread)[2]).toMatchObject({ id: C, status: 'queued' });
  });

  it('keeps the thread busy until every steered message has a result, folded into the turn or run on its own', async () => {
    const claude = await setup();
    const sent = claude.sendMessage(claude.thread, input('A'), A);
    void claude.sendMessage(claude.thread, input('B'), B);
    expect(await claude.steerTurn(claude.thread, B)).toMatchObject({ status: 'queued' });
    await flush();
    await claude.accept(A);
    await sent;
    for (const [id, text] of [
      [C, 'C'],
      [D, 'D'],
      [E, 'E'],
    ] as const)
      await claude.sendMessage(claude.thread, input(text), id);

    const steered = claude.steerTurn(claude.thread, B);
    const steeredAgain = claude.steerTurn(claude.thread, B);
    await flush();
    expect(claude.written()).toEqual([A, B]);
    await claude.accept(B);
    expect(await steered).toMatchObject({ id: B, status: 'accepted', turnId: A });
    expect(await steeredAgain).toMatchObject({ id: B, status: 'accepted' });
    await claude.finish([A, B]);
    expect(claude.events.filter((event) => event.kind === 'turn.completed').at(-1)).toMatchObject({
      turnId: A,
      messageIds: [A, B],
      outcome: 'completed',
    });
    expect(claude.written()).toEqual([A, B, C]);

    await claude.accept(C);
    const steeredD = claude.steerTurn(claude.thread, D);
    await flush();
    await claude.accept(D);
    await steeredD;
    let idle = false;
    const waiting = claude.waitUntilIdle(claude.thread).then(() => {
      idle = true;
    });
    await claude.finish([C]);
    expect(idle).toBe(false);
    expect(claude.written()).toEqual([A, B, C, D]);
    expect(() => claude.setThreadPermissions(claude.thread, 'fullAccess')).toThrow(
      'Stop the turn before changing permissions',
    );
    await claude.finish([D]);
    expect(claude.written()).toEqual([A, B, C, D, E]);
    await claude.accept(E);
    await claude.finish([E]);
    await waiting;
    expect(claude.messages(claude.thread).map(({ status, turnId }) => [status, turnId])).toEqual([
      ['accepted', A],
      ['accepted', A],
      ['accepted', C],
      ['accepted', C],
      ['accepted', E],
    ]);
  });

  it('lets steered input that outlives a stopped run finish, and stops it with a new interrupt', async () => {
    const claude = await setup();
    vi.useFakeTimers();
    await claude.start(A);
    await claude.sendMessage(claude.thread, input('B'), B);
    const steered = claude.steerTurn(claude.thread, B);
    await flush();
    await claude.accept(B);
    await steered;

    await claude.stopTurn(claude.thread);
    await claude.finish([A], interrupted);
    await vi.advanceTimersByTimeAsync(5000);
    await flush();
    expect(claude.launch().closed).toBe(false);
    expect(claude.events.some((event) => event.kind === 'session.exited')).toBe(false);

    await claude.stopTurn(claude.thread);
    expect(claude.launch().interrupts).toBe(2);
    await claude.finish([B], interrupted);
    await claude.waitUntilIdle(claude.thread);
    expect(
      claude.events.flatMap((event) => (event.kind === 'turn.completed' ? [[event.outcome, ...event.messageIds]] : [])),
    ).toEqual([
      ['interrupted', A],
      ['interrupted', B],
    ]);
  });
});
