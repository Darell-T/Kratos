import { describe, expect, it } from 'vitest';
import type { v2 } from '@kratos/codex';
import { input, setup, turn } from './codex-fixture';

describe('Codex plan mode', () => {
  it('sends the collaboration mode on every turn, leaves the sandbox to the thread permissions, and keeps the setting across restarts', async () => {
    const codex = await setup();
    const id = await codex.startThread('C:\\project-C', { plan: true, model: 'plan-model' });
    expect(codex.store.getCodexThread(id)).toMatchObject({ plan: true, model: 'plan-model' });
    expect(codex.store.getCodexThread('thread-A')?.plan).toBe(false);
    const run = async (turnId: string) => {
      const sent = codex.sendMessage(id, input(turnId));
      await codex.start(turnId, id);
      await sent;
    };

    await run('turn-A');
    expect(await codex.requests()).toMatchObject([
      {
        threadId: id,
        collaborationMode: {
          mode: 'plan',
          settings: { model: 'plan-model', reasoning_effort: null, developer_instructions: null },
        },
      },
    ]);
    expect(() => codex.setThreadPlanMode(id, false)).toThrow('Stop the turn before changing permissions');
    expect(codex.store.getCodexThread(id)?.plan).toBe(true);
    await codex.complete('turn-A', id, 'completed');

    codex.setThreadPlanMode(id, false);
    await run('turn-B');
    await codex.complete('turn-B', id, 'completed');
    codex.setThreadPermissions(id, 'fullAccess');
    await run('turn-C');
    await codex.complete('turn-C', id, 'completed');
    codex.setThreadPlanMode(id, true);
    await run('turn-D');
    await codex.complete('turn-D', id, 'completed');
    expect(
      (await codex.requests()).map(({ collaborationMode, sandboxPolicy, approvalPolicy }) => [
        collaborationMode?.mode,
        sandboxPolicy?.type,
        approvalPolicy,
      ]),
    ).toEqual([
      ['plan', 'workspaceWrite', 'on-request'],
      ['default', 'workspaceWrite', 'on-request'],
      ['default', 'dangerFullAccess', 'never'],
      ['plan', 'dangerFullAccess', 'never'],
    ]);

    const next = await codex.restart();
    await next.provider.resumeThread(id);
    const resumed = next.provider.sendMessage(id, input('E'));
    await next.control('test/echo', {});
    await next.control('test/settleTurn', { method: 'turn/start', threadId: id, result: { turn: turn('turn-E') } });
    await resumed;
    expect(await next.control('test/turnRequests', {})).toMatchObject([
      {
        method: 'turn/start',
        params: {
          collaborationMode: { mode: 'plan', settings: { model: 'plan-model' } },
          sandboxPolicy: { type: 'dangerFullAccess' },
          approvalPolicy: 'never',
        },
      },
    ]);
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
