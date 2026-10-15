import { describe, expect, it } from 'vitest';
import { input, setup, turn } from './codex-fixture';

const readOnly = { sandboxPolicy: { type: 'readOnly', networkAccess: false }, approvalPolicy: 'never' };

describe('Codex read-only threads', () => {
  it('starts, resumes and runs turns in the read-only sandbox without approvals, and changes only while idle', async () => {
    const codex = await setup();
    const id = await codex.startThread('C:\\project-C', { readOnly: true });
    expect(codex.store.getCodexThread(id)?.readOnly).toBe(true);
    expect(codex.store.getCodexThread('thread-A')?.readOnly).toBe(false);
    expect(
      (await codex.threadRequests()).map(({ method, params }) => [method, params.sandbox, params.approvalPolicy]),
    ).toEqual([
      ['thread/start', 'workspace-write', 'on-request'],
      ['thread/start', 'workspace-write', 'on-request'],
      ['thread/start', 'read-only', 'never'],
    ]);

    const sent = codex.sendMessage(id, input('A'));
    await codex.start('turn-A', id);
    await sent;
    expect(await codex.requests()).toMatchObject([{ threadId: id, ...readOnly }]);
    expect(() => codex.setThreadReadOnly(id, false)).toThrow('Stop the turn before changing permissions');
    await codex.complete('turn-A', id, 'completed');

    codex.setThreadReadOnly(id, false);
    const normal = codex.sendMessage(id, input('B'));
    await codex.start('turn-B', id);
    await normal;
    expect((await codex.requests()).at(-1)).toMatchObject({
      sandboxPolicy: { type: 'workspaceWrite' },
      approvalPolicy: 'on-request',
    });
    await codex.complete('turn-B', id, 'completed');

    codex.setThreadReadOnly(id, true);
    const next = await codex.restart();
    await next.provider.resumeThread(id);
    expect(await next.control('test/threadRequests', {})).toMatchObject([
      { method: 'thread/resume', params: { threadId: id, sandbox: 'read-only', approvalPolicy: 'never' } },
    ]);
    const resumed = next.provider.sendMessage(id, input('C'));
    await next.control('test/echo', {});
    await next.control('test/settleTurn', { method: 'turn/start', threadId: id, result: { turn: turn('turn-C') } });
    await resumed;
    expect(await next.control('test/turnRequests', {})).toMatchObject([{ method: 'turn/start', params: readOnly }]);
  });
});
