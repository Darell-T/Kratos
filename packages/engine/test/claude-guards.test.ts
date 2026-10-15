import { describe, expect, it } from 'vitest';
import { A, B, C, D, flush, folder, input, setup } from './claude-fixture';

const readOnly = { behavior: 'deny', message: 'This Kratos thread is read-only' };

describe('Claude read-only threads', () => {
  it('plans instead of acting, denies every request but questions, and keeps the setting across settings changes and restarts', async () => {
    const claude = await setup();
    expect(claude.store.getClaudeThread(claude.thread)?.readOnly).toBe(false);
    const created = await claude.startThread(folder, { readOnly: true });
    expect(claude.store.getClaudeThread(created)?.readOnly).toBe(true);
    claude.setThreadPermissions(claude.thread, 'fullAccess');
    claude.setThreadReadOnly(claude.thread, true);
    await claude.start(A);
    expect(claude.launch().options).toMatchObject({ permissionMode: 'plan' });
    expect(Object.keys(claude.launch().options)).not.toContain('allowDangerouslySkipPermissions');

    expect(await claude.ask('Bash', { command: 'rm -rf build' }).answer).toEqual(readOnly);
    expect(await claude.ask('ExitPlanMode', { plan: 'Delete the build folder' }).answer).toEqual(readOnly);
    expect(claude.approvalsWaiting(claude.thread)).toEqual([]);
    const questions = [{ question: 'Which folder?', header: 'Folder', options: [], multiSelect: false }];
    const asked = claude.ask('AskUserQuestion', { questions }, { toolUseID: 'question-1' });
    await flush();
    expect(claude.questionsWaiting(claude.thread)).toMatchObject([{ requestId: 'question-1', questions }]);
    expect(claude.approvalsWaiting(claude.thread)).toEqual([]);
    expect(claude.answerQuestion('question-1', { 'Which folder?': 'build' })).toBe(true);
    expect(await asked.answer).toMatchObject({ behavior: 'allow' });

    expect(() => claude.setThreadReadOnly(claude.thread, false)).toThrow('Stop the turn before changing permissions');
    await claude.finish([A]);
    claude.setThreadReadOnly(claude.thread, false);
    expect(claude.launch().closed).toBe(true);
    await claude.start(B);
    expect(claude.launch().options).toMatchObject({
      permissionMode: 'bypassPermissions',
      allowDangerouslySkipPermissions: true,
    });
    expect(await claude.ask('Bash', { command: 'rm -rf build' }).answer).toMatchObject({ behavior: 'allow' });
    await claude.finish([B]);

    claude.setThreadReadOnly(claude.thread, true);
    const restored = await claude.restart();
    expect(claude.store.getClaudeThread(claude.thread)).toMatchObject({ readOnly: true, permissions: 'fullAccess' });
    await restored.resumeThread(claude.thread);
    const sent = restored.sendMessage(claude.thread, input('C'), C);
    await flush();
    expect(claude.launch().options).toMatchObject({ cwd: folder, resume: claude.thread, permissionMode: 'plan' });
    expect(await claude.ask('Write', { file_path: 'a.ts' }).answer).toEqual(readOnly);
    await claude.accept(C);
    await sent;
  });
});

describe('Claude turn changes', () => {
  it('reports the files each turn edited and whether a command ran after the last edit', async () => {
    const claude = await setup();
    await claude.start(A);
    expect(await claude.useTool('PostToolUse', 'Edit', { file_path: 'a.ts' })).toEqual([{ continue: true }]);
    await claude.useTool('PostToolUse', 'Bash', { command: 'npm test' });
    await claude.useTool('PostToolUse', 'Write', { file_path: 'b.ts' });
    await claude.useTool('PostToolUse', 'Edit', { file_path: 'a.ts' });
    expect(await claude.useTool('PostToolUseFailure', 'Edit', { file_path: 'c.ts' })).toEqual([{ continue: true }]);
    await claude.useTool('PostToolUse', 'Read', { file_path: 'd.ts' });
    await claude.finish([A]);

    await claude.start(B);
    await claude.useTool('PostToolUse', 'NotebookEdit', { notebook_path: 'n.ipynb' });
    await claude.useTool('PostToolUse', 'MultiEdit', { file_path: 'm.ts' });
    await claude.useTool('PostToolUseFailure', 'PowerShell', { command: 'npm test' });
    await claude.finish([B]);

    await claude.start(C);
    await claude.useTool('PostToolUse', 'Read', { file_path: 'a.ts' });
    await claude.finish([C]);

    await claude.start(D);
    await claude.useTool('PostToolUse', 'Edit', { file_path: 'e.ts' });
    await claude.finish([D]);

    expect(
      claude.events.flatMap((event) => (event.kind === 'turn.completed' ? [[event.turnId, event.changes]] : [])),
    ).toEqual([
      [A, { files: ['a.ts', 'b.ts'], checked: false }],
      [B, { files: ['n.ipynb', 'm.ts'], checked: true }],
      [C, { files: [], checked: true }],
      [D, { files: ['e.ts'], checked: false }],
    ]);
  });
});
