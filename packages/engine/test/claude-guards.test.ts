import { describe, expect, it } from 'vitest';
import { A, B, C, D, E, flush, folder, input, setup } from './claude-fixture';

const planOn = {
  behavior: 'deny',
  message: 'Plan mode is on. The user turns it off in Kratos to carry out the plan.',
};
const F = '00000000-0000-4000-8000-00000000000f';

describe('Claude plan mode', () => {
  it('launches in the native plan mode, denies only ExitPlanMode, and keeps the setting across settings changes and restarts', async () => {
    const claude = await setup();
    expect(claude.store.getClaudeThread(claude.thread)?.plan).toBe(false);
    const created = await claude.startThread(folder, { plan: true });
    expect(claude.store.getClaudeThread(created)?.plan).toBe(true);

    claude.setThreadPermissions(claude.thread, 'fullAccess');
    claude.setThreadPlanMode(claude.thread, true);
    await claude.start(A);
    expect(claude.launch().options).toMatchObject({ permissionMode: 'plan' });
    expect(Object.keys(claude.launch().options)).not.toContain('allowDangerouslySkipPermissions');
    expect(await claude.ask('ExitPlanMode', { plan: 'Delete the build folder' }).answer).toEqual(planOn);
    expect(claude.approvalsWaiting(claude.thread)).toEqual([]);
    await claude.finish([A]);

    claude.setThreadPermissions(claude.thread, 'askMe');
    await claude.start(B);
    expect(claude.launch().options).toMatchObject({ permissionMode: 'plan' });
    const command = claude.ask('Bash', { command: 'npm test' }, { toolUseID: 'command-1' });
    await flush();
    expect(await claude.ask('ExitPlanMode', { plan: 'Delete the build folder' }).answer).toEqual(planOn);
    expect(claude.approvalsWaiting(claude.thread)).toMatchObject([
      { requestId: 'command-1', toolName: 'Bash', input: { command: 'npm test' } },
    ]);
    expect(claude.decideApproval('command-1', 'allow')).toBe(true);
    expect(await command.answer).toMatchObject({ behavior: 'allow' });
    const questions = [{ question: 'Which folder?', header: 'Folder', options: [], multiSelect: false }];
    const asked = claude.ask('AskUserQuestion', { questions }, { toolUseID: 'question-1' });
    await flush();
    expect(claude.questionsWaiting(claude.thread)).toMatchObject([{ requestId: 'question-1', questions }]);
    expect(claude.approvalsWaiting(claude.thread)).toEqual([]);
    expect(claude.answerQuestion('question-1', { 'Which folder?': 'build' })).toBe(true);
    expect(await asked.answer).toMatchObject({ behavior: 'allow' });

    expect(() => claude.setThreadPlanMode(claude.thread, false)).toThrow('Stop the turn before changing permissions');
    expect(claude.store.getClaudeThread(claude.thread)?.plan).toBe(true);
    await claude.finish([B]);
    claude.setThreadPlanMode(claude.thread, false);
    expect(claude.launch().closed).toBe(true);
    await claude.start(C);
    expect(claude.launch().options).toMatchObject({ permissionMode: 'default' });
    const exit = claude.ask('ExitPlanMode', { plan: 'Delete the build folder' }, { toolUseID: 'exit-1' });
    await flush();
    expect(claude.approvalsWaiting(claude.thread)).toMatchObject([{ requestId: 'exit-1', toolName: 'ExitPlanMode' }]);
    expect(claude.decideApproval('exit-1', 'allow')).toBe(true);
    expect(await exit.answer).toMatchObject({ behavior: 'allow' });
    await claude.finish([C]);

    claude.setThreadPlanMode(claude.thread, true);
    const restored = await claude.restart();
    expect(claude.store.getClaudeThread(claude.thread)).toMatchObject({ plan: true, permissions: 'askMe' });
    await restored.resumeThread(claude.thread);
    const sent = restored.sendMessage(claude.thread, input('D'), D);
    await flush();
    expect(claude.launch().options).toMatchObject({ cwd: folder, resume: claude.thread, permissionMode: 'plan' });
    expect(await claude.ask('ExitPlanMode', { plan: 'Delete the build folder' }).answer).toEqual(planOn);
    await claude.accept(D);
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
    expect(await claude.useTool('PostToolUse', 'Edit', { file_path: 42 })).toEqual([{ continue: true }]);
    await claude.finish([C]);

    await claude.start(D);
    await claude.useTool('PostToolUse', 'Edit', { file_path: 'e.ts' });
    await claude.finish([D]);

    await claude.start(E);
    await claude.useTool('PostToolUse', 'Edit', { file_path: 'f.ts' });
    await claude.exit(new Error('Claude Code process exited with code 3'));
    await flush();
    expect(claude.events.filter((event) => event.kind === 'session.exited')).toMatchObject([
      { changes: { files: ['f.ts'], checked: false } },
    ]);
    await claude.resumeQueue(claude.thread);
    await claude.useTool('PostToolUse', 'Edit', { file_path: 'late.ts' });
    await claude.start(F);
    await claude.useTool('PostToolUse', 'Monitor', { command: 'npm run dev' });
    await claude.finish([F]);

    expect(
      claude.events.flatMap((event) => (event.kind === 'turn.completed' ? [[event.turnId, event.changes]] : [])),
    ).toEqual([
      [A, { files: ['a.ts', 'b.ts'], checked: false }],
      [B, { files: ['n.ipynb', 'm.ts'], checked: true }],
      [C, { files: [], checked: true }],
      [D, { files: ['e.ts'], checked: false }],
      [F, { files: [], checked: true }],
    ]);
  });
});
