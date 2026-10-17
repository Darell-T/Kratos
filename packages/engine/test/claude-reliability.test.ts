import { describe, expect, it } from 'vitest';
import type { ClaudeEvent } from '../src/providers/claude';
import { A, B, C, D, E, flush, folder, input, setup } from './claude-fixture';

describe('Claude delivery when Claude Code exits', () => {
  it('fails a message the process never started, and judges later losses by the transcript, only for messages the lost process received', async () => {
    const claude = await setup();
    const failed = claude.sendMessage(claude.thread, input('A'), A).catch((error: unknown) => error);
    await flush();
    const notLoggedIn = 'Claude Code process exited with code 1. stderr: Not logged in';
    await claude.exit(new Error(notLoggedIn));
    expect(await failed).toMatchObject({ message: notLoggedIn });
    expect(claude.messages(claude.thread)).toMatchObject([{ id: A, status: 'failed', error: notLoggedIn }]);
    expect(claude.events).toContainEqual({ kind: 'session.exited', threadId: claude.thread, message: notLoggedIn });

    const retried = claude.retryMessage(claude.thread, A);
    await flush();
    expect(claude.launches).toHaveLength(2);
    expect(claude.launch().options).toMatchObject({ sessionId: claude.thread });
    await claude.init();
    await claude.accept(A);
    expect(await retried).toMatchObject({ id: A, status: 'accepted' });

    for (const [id, text] of [
      [B, 'B'],
      [C, 'C'],
      [D, 'D'],
    ] as const)
      await claude.sendMessage(claude.thread, input(text), id);
    const steeredB = claude.steerTurn(claude.thread, B).catch((error: unknown) => error);
    const steeredC = claude.steerTurn(claude.thread, C).catch((error: unknown) => error);
    await flush();
    claude.record(B);
    const crashed = 'Claude Code process exited with code 3';
    await claude.exit(new Error(crashed));
    await flush();
    expect(await steeredB).toMatchObject({ id: B, status: 'accepted', turnId: A });
    expect(await steeredC).toMatchObject({ message: crashed });
    expect(claude.messages(claude.thread)).toMatchObject([
      { id: A, status: 'accepted' },
      { id: B, status: 'accepted' },
      { id: C, status: 'unknown', error: crashed },
      { id: D, status: 'queued' },
    ]);
    await expect(claude.retryMessage(claude.thread, C)).rejects.toThrow(
      'Only messages confirmed not to have executed can be retried',
    );
    await expect(claude.resumeQueue(claude.thread)).rejects.toThrow('Resolve undelivered messages');

    expect(await claude.discardMessage(claude.thread, C)).toMatchObject({ status: 'cancelled' });
    const resumed = claude.resumeQueue(claude.thread);
    await flush();
    expect(claude.launches).toHaveLength(3);
    expect(claude.launch().options).toMatchObject({ resume: claude.thread });
    expect(claude.written()).toEqual([D]);
    await claude.accept(D);
    await resumed;

    await claude.sendMessage(claude.thread, input('E'), E);
    const readTranscript = claude.holdTranscripts();
    await claude.exit(new Error(crashed));
    const resumedBeforeRead = claude.resumeQueue(claude.thread);
    await flush();
    expect(claude.launches).toHaveLength(4);
    expect(claude.written()).toEqual([E]);
    readTranscript();
    await flush();
    expect(claude.launch().closed).toBe(false);
    expect(claude.messages(claude.thread).at(-1)).toMatchObject({ id: E, status: 'submitting' });
    await claude.accept(E);
    await resumedBeforeRead;
  });
});

describe('Claude restart recovery and settings', () => {
  it('restores a thread after an engine restart without resending delivered work', async () => {
    const claude = await setup();
    const restored = await claude.restart();
    const settings = claude.store.getClaudeThread(claude.thread);
    if (!settings) throw new Error('Thread was not saved');
    claude.store.saveClaudeThread({ ...settings, runningTurn: A });
    for (const [id, text] of [
      [A, 'A'],
      [B, 'B'],
      [C, 'C'],
      [D, 'D'],
    ] as const)
      claude.store.addClaudeMessage(id, claude.thread, input(text));
    claude.store.updateClaudeMessage(A, 'accepted', A);
    claude.store.updateClaudeMessage(B, 'submitting', A);
    claude.store.updateClaudeMessage(C, 'submitting', A);
    claude.record(A);
    claude.record(B);
    const events: ClaudeEvent[] = [];
    restored.onEvent((event) => {
      events.push(event);
    });

    await Promise.all([restored.resumeThread(claude.thread), restored.resumeThread(claude.thread)]);
    expect(restored.messages(claude.thread)).toMatchObject([
      { id: A, status: 'accepted' },
      { id: B, status: 'accepted', turnId: A },
      { id: C, status: 'unknown', error: 'Delivery was interrupted' },
      { id: D, status: 'queued' },
    ]);
    expect(events.filter((event) => event.kind === 'message.updated' && event.message.id === B)).toHaveLength(1);
    expect(claude.store.getClaudeThread(claude.thread)).toMatchObject({
      runningTurn: null,
      pausedError: 'Last turn was interrupted before completion',
    });
    await expect(restored.waitUntilIdle(claude.thread)).rejects.toThrow('Last turn was interrupted before completion');
    expect(claude.launches).toEqual([]);

    expect(await restored.discardMessage(claude.thread, C)).toMatchObject({ status: 'cancelled' });
    const resumed = restored.resumeQueue(claude.thread);
    await flush();
    expect(claude.launch().options).toMatchObject({ resume: claude.thread });
    expect(claude.written()).toEqual([D]);
    await claude.accept(D);
    await resumed;
  });

  it('relaunches an idle session when its settings change, bypassing permissions only for full access', async () => {
    const claude = await setup();
    await claude.start(A);
    await claude.finish([A]);
    claude.setThreadModel(claude.thread, 'claude-opus-test');
    expect(claude.launch().closed).toBe(true);
    await claude.start(B);
    expect(claude.launches).toHaveLength(2);
    expect(claude.launch().options).toMatchObject({
      model: 'claude-opus-test',
      resume: claude.thread,
      permissionMode: 'default',
    });

    expect(() => claude.setThreadPermissions(claude.thread, 'fullAccess')).toThrow(
      'Stop the turn before changing permissions',
    );
    claude.setThreadModel(claude.thread, 'claude-haiku-test');
    expect(claude.launch().closed).toBe(false);
    await claude.finish([B]);
    expect(claude.launch().closed).toBe(true);

    claude.setThreadPermissions(claude.thread, 'fullAccess');
    await claude.start(C);
    expect(claude.launch().options).toMatchObject({
      model: 'claude-haiku-test',
      permissionMode: 'bypassPermissions',
      allowDangerouslySkipPermissions: true,
    });
    await claude.finish([C]);

    claude.setThreadPermissions(claude.thread, 'approveForMe');
    await claude.start(D);
    expect(claude.launch().options).toMatchObject({ permissionMode: 'auto' });
    expect(Object.keys(claude.launch().options)).not.toContain('allowDangerouslySkipPermissions');
    expect(claude.launches).toHaveLength(4);
    expect(claude.store.getClaudeThread(claude.thread)).toMatchObject({
      model: 'claude-haiku-test',
      permissions: 'approveForMe',
    });
  });

  it('rejects unknown threads and invalid input, shares duplicate sends, and closes every thread at shutdown', async () => {
    const claude = await setup();
    await expect(claude.sendMessage('missing', input('A'))).rejects.toThrow(
      'Start or resume thread missing before using it',
    );
    await expect(claude.sendMessage(claude.thread, [])).rejects.toThrow();
    await expect(claude.sendMessage(claude.thread, input('A'), 'message-A')).rejects.toThrow();
    expect(claude.launches).toEqual([]);

    const first = claude.sendMessage(claude.thread, input('A'), A);
    const duplicate = claude.sendMessage(claude.thread, input('A'), A);
    await flush();
    await claude.init();
    await claude.accept(A);
    expect(await first).toEqual(await duplicate);
    await expect(claude.sendMessage(claude.thread, input('Different'), A)).rejects.toThrow('different submission');
    expect(claude.written()).toEqual([A]);

    await claude.sendMessage(claude.thread, input('B'), B);
    const steered = claude.steerTurn(claude.thread, B).catch((error: unknown) => error);
    const approval = claude.ask('Bash', { command: 'npm test' });
    await flush();
    const idle = claude.waitUntilIdle(claude.thread).catch((error: unknown) => error);
    await claude.close();
    expect(await idle).toMatchObject({ message: 'Claude provider closed' });
    expect(await steered).toMatchObject({ message: 'Claude provider closed' });
    expect(await approval.answer).toEqual({ behavior: 'deny', message: 'The user declined this action' });
    expect(claude.launch().closed).toBe(true);
    expect(claude.messages(claude.thread)).toMatchObject([
      { id: A, status: 'accepted' },
      { id: B, status: 'unknown', error: 'Claude provider closed' },
    ]);
    await expect(claude.sendMessage(claude.thread, input('C'))).rejects.toThrow('Claude provider is closed');
    await expect(claude.startThread('C:\\project-B')).rejects.toThrow('Claude provider is closed');
  });

  it('reports discovery from Claude Code without sending a prompt, and recognizes each kind of sign-in', async () => {
    const claude = await setup();
    claude.cli.models = [{ value: 'sonnet', displayName: 'Sonnet', description: 'Everyday coding' }];
    claude.cli.commands = [{ name: 'review', description: 'Review the diff', argumentHint: '' }];
    claude.cli.plugins = [{ name: 'pstack', path: 'C:\\plugins\\pstack', source: 'marketplace' }];
    expect(await claude.providerModels()).toEqual(
      new Map([['sonnet', { id: 'sonnet', name: 'Sonnet', details: 'Everyday coding' }]]),
    );
    expect(await claude.findSkills(folder)).toEqual(
      new Map([['review', { name: 'review', details: 'Review the diff' }]]),
    );
    expect(await claude.findPlugins(folder)).toEqual(
      new Map([['C:\\plugins\\pstack', { name: 'pstack', path: 'C:\\plugins\\pstack' }]]),
    );
    expect(claude.launch().options).toMatchObject({ cwd: folder, settingSources: ['user', 'project', 'local'] });
    expect(claude.launches.every((launch) => launch.written.length === 0 && launch.closed)).toBe(true);

    expect(await claude.isSignedIn()).toBe(false);
    claude.cli.account = { email: 'dev@example.com', subscriptionType: 'Claude Pro', apiProvider: 'firstParty' };
    expect(await claude.isSignedIn()).toBe(true);
    claude.cli.account = { apiKeySource: 'ANTHROPIC_API_KEY', apiProvider: 'firstParty' };
    expect(await claude.isSignedIn()).toBe(true);
    claude.cli.account = { apiProvider: 'bedrock' };
    expect(await claude.isSignedIn()).toBe(true);
  });
});
