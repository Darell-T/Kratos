import { describe, expect, it } from 'vitest';
import type { v2 } from '@kratos/codex';
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
    const commandAnswer = codex.answers();
    await codex.ask('item/commandExecution/requestApproval', 'write', {
      kind: 'command',
      threadId: id,
      turnId: 'turn-A',
      itemId: 'command-A',
      startedAtMs: 100,
      environmentId: null,
      command: 'rm -rf build',
      reason: null,
      cwd: 'C:\\project-C',
    });
    await codex.flush();
    expect(codex.approvalsWaiting(id)).toEqual([]);
    expect(await commandAnswer).toMatchObject({ id: 'write', result: { decision: 'cancel' } });
    const permissionAnswer = codex.answers();
    await codex.ask('item/permissions/requestApproval', 'widen', {
      threadId: id,
      turnId: 'turn-A',
      itemId: 'permission-A',
      environmentId: null,
      startedAtMs: 100,
      cwd: 'C:\\project-C',
      reason: null,
      permissions: { network: null, fileSystem: null },
    });
    await codex.flush();
    expect(codex.permissionsWaiting(id)).toEqual([]);
    expect(await permissionAnswer).toMatchObject({ id: 'widen', result: { permissions: {}, scope: 'turn' } });
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

function fileChange(id: string, status: v2.PatchApplyStatus, ...paths: string[]): v2.ThreadItem {
  return {
    type: 'fileChange',
    id,
    status,
    changes: paths.map((path) => ({ path, kind: { type: 'update', move_path: null }, diff: '+new' })),
  };
}

function command(id: string, status: v2.CommandExecutionStatus): v2.ThreadItem {
  return {
    type: 'commandExecution',
    id,
    pluginId: null,
    scriptPath: null,
    command: 'npm test',
    cwd: 'C:\\project',
    processId: null,
    source: 'agent',
    status,
    commandActions: [],
    aggregatedOutput: null,
    exitCode: null,
    durationMs: null,
  };
}

describe('Codex turn changes', () => {
  it('reports the files each thread edited per turn and whether a command ran after the last edit, ignoring declined work', async () => {
    const codex = await setup();
    const done = (threadId: string, turnId: string, item: v2.ThreadItem) =>
      codex.emit('item/completed', { threadId, turnId, completedAtMs: 102000, item });
    const begin = async (threadId: string, turnId: string) => {
      const sent = codex.sendMessage(threadId, input(turnId));
      await codex.start(turnId, threadId);
      await sent;
    };
    const end = (threadId: string, turnId: string) => codex.complete(turnId, threadId, 'completed');

    await begin('thread-A', 'turn-A');
    await begin('thread-B', 'turn-B');
    await done('thread-A', 'turn-A', fileChange('f1', 'completed', 'a.ts'));
    await done('thread-A', 'turn-A', command('c1', 'completed'));
    await done('thread-B', 'turn-B', fileChange('f2', 'completed', 'other.ts'));
    await done('thread-A', 'turn-A', fileChange('f3', 'completed', 'b.ts', 'a.ts'));
    await done('thread-A', 'turn-A', fileChange('f4', 'declined', 'c.ts'));
    await done('thread-A', 'turn-A', command('c2', 'declined'));
    await end('thread-A', 'turn-A');
    await end('thread-B', 'turn-B');

    await begin('thread-A', 'turn-C');
    await done('thread-A', 'turn-C', fileChange('f5', 'declined', 'd.ts'));
    await done('thread-A', 'turn-C', command('c3', 'declined'));
    await end('thread-A', 'turn-C');

    await begin('thread-A', 'turn-D');
    await done('thread-A', 'turn-D', fileChange('f6', 'completed', 'n.ts'));
    await done('thread-A', 'turn-D', command('c4', 'failed'));
    await end('thread-A', 'turn-D');
    await done('thread-A', 'turn-D', fileChange('late', 'completed', 'late.ts'));

    const sent = codex.sendMessage('thread-A', input('turn-E'));
    await done('thread-A', 'turn-E', fileChange('f7', 'completed', 'e.ts'));
    await codex.start('turn-E', 'thread-A', 'completed');
    await sent;

    expect(
      codex.events.flatMap((event) => (event.kind === 'turn.completed' ? [[event.turn.id, event.changes]] : [])),
    ).toEqual([
      ['turn-A', { files: ['a.ts', 'b.ts'], checked: false }],
      ['turn-B', { files: ['other.ts'], checked: false }],
      ['turn-C', { files: [], checked: true }],
      ['turn-D', { files: ['n.ts'], checked: true }],
      ['turn-E', { files: ['e.ts'], checked: false }],
    ]);
  });
});
