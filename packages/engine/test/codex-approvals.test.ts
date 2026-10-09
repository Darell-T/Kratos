import { fileURLToPath } from 'node:url';
import { afterEach, describe, expect, it } from 'vitest';
import {
  connectCodex,
  type CodexClient,
  type NotificationMethod,
  type NotificationParams,
  type RequestId,
  type ServerRequestMethod,
  type ServerRequestParams,
  type v2,
} from '@kratos/codex';
import { createCodexApprovals, type Permission } from '../src/providers/codex-approvals';

// Only the fake's test controls are outside the generated Codex protocol.
interface FakeControls {
  request(method: string, params: unknown): Promise<unknown>;
  onNotification(method: string, handler: (params: unknown) => void): () => void;
}

const clients: CodexClient[] = [];
afterEach(async () => {
  await Promise.all(clients.splice(0).map((client) => client.close()));
});

async function setup(permission: Permission | undefined = 'askMe') {
  const client = await connectCodex({
    command: process.execPath,
    args: [fileURLToPath(new URL('../../codex/test/fake-codex.mjs', import.meta.url))],
  });
  clients.push(client);
  const controls = client as unknown as FakeControls;
  const approvals = createCodexApprovals(client, (threadId) => (threadId === 'unknown' ? undefined : permission));
  const answers: unknown[] = [];
  controls.onNotification('test/answeredWithId', (answer) => answers.push(answer));

  return {
    client,
    approvals,
    answers,
    ask: <M extends ServerRequestMethod>(method: M, requestId: RequestId, params: ServerRequestParams<M>) => {
      return controls.request('test/ask', { method, requestId, params });
    },
    emit: <M extends NotificationMethod>(method: M, params: NotificationParams<M>) => {
      return controls.request('test/emit', { method, params });
    },
    flush: async () => {
      // Let a settled approval write its answer before placing the echo barrier on the same pipe.
      await Promise.resolve();
      await controls.request('test/echo', {});
    },
    crash: () => controls.request('test/exit', { code: 3 }).catch(() => undefined),
  };
}

function commandParams(
  overrides: Partial<v2.CommandExecutionRequestApprovalParams> = {},
): v2.CommandExecutionRequestApprovalParams {
  return {
    kind: 'command',
    threadId: 'thread-A',
    turnId: 'turn-A',
    itemId: 'command-item',
    startedAtMs: 100,
    environmentId: null,
    command: 'npm test',
    reason: 'Run tests',
    cwd: 'C:\\project',
    ...overrides,
  };
}

function fileParams(overrides: Partial<v2.FileChangeRequestApprovalParams> = {}): v2.FileChangeRequestApprovalParams {
  return { threadId: 'thread-A', turnId: 'turn-A', itemId: 'file-item', startedAtMs: 100, ...overrides };
}

function fileItem(changes: v2.FileUpdateChange[], threadId = 'thread-A'): v2.ItemStartedNotification {
  return {
    threadId,
    turnId: 'turn-A',
    startedAtMs: 100,
    item: { type: 'fileChange', id: 'file-item', changes, status: 'inProgress' },
  };
}

describe('Codex approvals through the client transport', () => {
  it('answers overlapping requests independently, including numeric and string IDs', async () => {
    const { ask, approvals, answers, flush } = await setup();
    await ask('item/commandExecution/requestApproval', 42, commandParams());
    await ask('item/commandExecution/requestApproval', '42', commandParams());

    expect(approvals.approvalsWaiting('thread-A').map((request) => request.requestId)).toEqual([42, '42']);
    expect(answers).toEqual([]);
    expect(approvals.decideCommand('42', 'decline')).toBe(true);
    await flush();
    expect(answers).toEqual([{ id: '42', result: { decision: 'decline' } }]);
    expect(approvals.commandWaiting(42)?.command).toBe('npm test');
    expect(approvals.decideCommand('42', 'accept')).toBe(false);

    expect(approvals.decideCommand(42, 'accept')).toBe(true);
    await flush();
    expect(answers).toEqual([
      { id: '42', result: { decision: 'decline' } },
      { id: 42, result: { decision: 'accept' } },
    ]);
    expect(approvals.approvalsWaiting('thread-A')).toEqual([]);
  });

  it('lists command and file approvals for their owning thread', async () => {
    const { ask, approvals } = await setup();
    await ask('item/commandExecution/requestApproval', 'command-A', commandParams());
    await ask('item/fileChange/requestApproval', 'file-A', fileParams());
    await ask('item/commandExecution/requestApproval', 'command-B', commandParams({ threadId: 'thread-B' }));

    expect(approvals.approvalsWaiting('thread-A')).toMatchObject([
      { requestId: 'command-A', kind: 'command', threadId: 'thread-A', turnId: 'turn-A', itemId: 'command-item' },
      { requestId: 'file-A', kind: 'file', threadId: 'thread-A', turnId: 'turn-A', itemId: 'file-item' },
    ]);
    expect(approvals.approvalsWaiting('thread-B').map((request) => request.requestId)).toEqual(['command-B']);
  });

  it.each(['before', 'after'] as const)(
    'attaches file changes when the item arrives %s the approval',
    async (order) => {
      const { ask, emit, approvals } = await setup();
      const changes: v2.FileUpdateChange[] = [
        { path: 'new.ts', kind: { type: 'add' }, diff: '+new' },
        { path: 'old.ts', kind: { type: 'delete' }, diff: '-old' },
        { path: 'move.ts', kind: { type: 'update', move_path: 'renamed.ts' }, diff: '-before\n+after' },
      ];
      if (order === 'before') await emit('item/started', fileItem(changes));
      await ask('item/fileChange/requestApproval', 'files', fileParams({ reason: 'Update project' }));
      if (order === 'after') {
        expect(approvals.fileWaiting('files')?.changes).toBeNull();
        await emit('item/started', fileItem(changes));
      }

      expect(approvals.fileWaiting('files')).toMatchObject({ reason: 'Update project', changes });
      // A matching item ID in another thread must not overwrite this approval's files.
      await emit('item/started', fileItem([], 'thread-B'));
      expect(approvals.fileWaiting('files')?.changes).toEqual(changes);
    },
  );

  it('answers overlapping file approvals without resolving the other request', async () => {
    const { ask, approvals, answers, flush } = await setup();
    await ask('item/fileChange/requestApproval', 'first', fileParams());
    await ask('item/fileChange/requestApproval', 'second', fileParams({ itemId: 'other-item' }));
    expect(approvals.decideFile('second', 'acceptForSession')).toBe(true);
    await flush();
    expect(answers).toEqual([{ id: 'second', result: { decision: 'acceptForSession' } }]);
    expect(approvals.fileWaiting('first')).toBeDefined();
    expect(approvals.decideFile('second', 'decline')).toBe(false);
  });

  it('removes an externally resolved request without sending a late answer', async () => {
    const { ask, emit, approvals, answers, flush } = await setup();
    await ask('item/fileChange/requestApproval', 51, fileParams());
    await emit('serverRequest/resolved', { threadId: 'thread-A', requestId: 51 });
    await flush();
    expect(approvals.fileWaiting(51)).toBeUndefined();
    expect(approvals.decideFile(51, 'accept')).toBe(false);
    expect(answers).toEqual([]);
  });

  it('clears only the completed turn and discards its cached file details', async () => {
    const { ask, emit, approvals, answers, flush } = await setup();
    await emit('item/started', fileItem([{ path: 'old.ts', kind: { type: 'delete' }, diff: '-old' }]));
    await ask('item/fileChange/requestApproval', 'old-files', fileParams());
    await ask('item/commandExecution/requestApproval', 'old-command', commandParams());
    await ask('item/commandExecution/requestApproval', 'new-command', commandParams({ turnId: 'turn-B' }));
    await emit('turn/completed', {
      threadId: 'thread-A',
      turn: {
        id: 'turn-A',
        status: 'interrupted',
        items: [],
        itemsView: 'full',
        error: null,
        startedAt: null,
        completedAt: null,
        durationMs: null,
      },
    });
    await flush();
    expect(approvals.approvalsWaiting('thread-A').map((request) => request.requestId)).toEqual(['new-command']);
    expect(answers).toEqual(
      expect.arrayContaining([
        { id: 'old-files', result: { decision: 'cancel' } },
        { id: 'old-command', result: { decision: 'cancel' } },
      ]),
    );
    await ask('item/fileChange/requestApproval', 'new-files', fileParams({ turnId: 'turn-B' }));
    expect(approvals.fileWaiting('new-files')?.changes).toBeNull();
  });

  it('clears a closed thread without affecting another thread', async () => {
    const { ask, emit, approvals } = await setup();
    await ask('item/commandExecution/requestApproval', 'A', commandParams());
    await ask('item/commandExecution/requestApproval', 'B', commandParams({ threadId: 'thread-B' }));
    await emit('thread/closed', { threadId: 'thread-A' });
    expect(approvals.approvalsWaiting('thread-A')).toEqual([]);
    expect(approvals.commandWaiting('B')).toBeDefined();
  });

  it('clears approvals when the provider crashes', async () => {
    const { ask, approvals, crash } = await setup();
    await ask('item/commandExecution/requestApproval', 'command', commandParams());
    await ask('item/fileChange/requestApproval', 'files', fileParams());
    await crash();
    expect(approvals.approvalsWaiting('thread-A')).toEqual([]);
    expect(approvals.decideCommand('command', 'accept')).toBe(false);
    expect(approvals.decideFile('files', 'accept')).toBe(false);
  });

  it('accepts full access requests without leaving pending approvals', async () => {
    const { ask, approvals, answers, flush } = await setup('fullAccess');
    await ask('item/commandExecution/requestApproval', 'command', commandParams());
    await ask('item/fileChange/requestApproval', 'files', fileParams());
    await flush();
    expect(approvals.approvalsWaiting('thread-A')).toEqual([]);
    expect(answers).toEqual([
      { id: 'command', result: { decision: 'accept' } },
      { id: 'files', result: { decision: 'accept' } },
    ]);
  });

  it('cancels requests for unknown threads instead of granting permission', async () => {
    const { ask, approvals, answers, flush } = await setup();
    await ask('item/commandExecution/requestApproval', 'command', commandParams({ threadId: 'unknown' }));
    await ask('item/fileChange/requestApproval', 'files', fileParams({ threadId: 'unknown' }));
    await flush();
    expect(approvals.approvalsWaiting('unknown')).toEqual([]);
    expect(answers).toEqual([
      { id: 'command', result: { decision: 'cancel' } },
      { id: 'files', result: { decision: 'cancel' } },
    ]);
  });

  it('does not silently accept a request delivered to Kratos in auto-review mode', async () => {
    const { ask, approvals, answers, flush } = await setup('approveForMe');
    await ask('item/commandExecution/requestApproval', 'review', commandParams());
    await flush();
    expect(answers).toEqual([]);
    expect(approvals.commandWaiting('review')).toBeDefined();
  });
});
