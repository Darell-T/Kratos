import { describe, expect, it, vi } from 'vitest';
import { A, B, flush, input, sessionRule, setup } from './claude-fixture';

describe('Claude approvals and user requests', () => {
  it('routes tool approvals to Kratos and answers each request exactly once', async () => {
    const claude = await setup();
    await claude.start(A);

    const bash = claude.ask(
      'Bash',
      { command: 'npm test' },
      {
        toolUseID: 'tool-bash',
        suggestions: [sessionRule, { ...sessionRule, destination: 'localSettings' }],
        title: 'Claude wants to run npm test',
        decisionReason: 'Not in the allow list',
        blockedPath: 'C:\\outside',
      },
    );
    await flush();
    const approval = {
      requestId: 'tool-bash',
      threadId: claude.thread,
      turnId: A,
      toolName: 'Bash',
      input: { command: 'npm test' },
      title: 'Claude wants to run npm test',
      description: null,
      reason: 'Not in the allow list',
      blockedPath: 'C:\\outside',
      canRememberForSession: true,
    };
    expect(claude.approvalsWaiting(claude.thread)).toEqual([approval]);
    expect(claude.events).toContainEqual({ kind: 'approvals.changed', threadId: claude.thread, approvals: [approval] });
    expect(claude.decideApproval('tool-bash', 'allowForSession')).toBe(true);
    expect(await bash.answer).toEqual({
      behavior: 'allow',
      updatedInput: { command: 'npm test' },
      updatedPermissions: [sessionRule],
    });
    expect(claude.decideApproval('tool-bash', 'deny')).toBe(false);

    const write = claude.ask(
      'Write',
      { file_path: 'a.ts' },
      { toolUseID: 'tool-write', suggestions: [sessionRule], suppressAlwaysAllowRule: true },
    );
    const edit = claude.ask('Edit', { file_path: 'b.ts' }, { toolUseID: 'tool-edit' });
    await flush();
    expect(claude.approvalsWaiting(claude.thread).map((item) => [item.requestId, item.canRememberForSession])).toEqual([
      ['tool-write', false],
      ['tool-edit', false],
    ]);
    expect(claude.decideApproval('tool-write', 'allowForSession')).toBe(false);
    expect(claude.decideApproval('tool-write', 'deny')).toBe(true);
    expect(await write.answer).toEqual({ behavior: 'deny', message: 'The user declined this action' });
    expect(claude.decideApproval('tool-edit', 'allow')).toBe(true);
    expect(await edit.answer).toEqual({ behavior: 'allow', updatedInput: { file_path: 'b.ts' } });

    const aborted = claude.ask('Read', { file_path: 'c.ts' }, { toolUseID: 'tool-aborted' });
    const cleared = claude.ask('Read', { file_path: 'd.ts' }, { toolUseID: 'tool-cleared' });
    await flush();
    aborted.abort();
    expect(await aborted.answer).toMatchObject({ behavior: 'deny' });
    expect(claude.approvalsWaiting(claude.thread).map((item) => item.requestId)).toEqual(['tool-cleared']);
    await claude.finish([A]);
    expect(await cleared.answer).toMatchObject({ behavior: 'deny' });
    expect(claude.decideApproval('tool-cleared', 'allow')).toBe(false);
    expect(claude.events.filter((event) => event.kind === 'approvals.changed').at(-1)).toMatchObject({ approvals: [] });

    claude.setThreadPermissions(claude.thread, 'fullAccess');
    await claude.start(B);
    expect(await claude.ask('Bash', { command: 'rm -rf build' }).answer).toEqual({
      behavior: 'allow',
      updatedInput: { command: 'rm -rf build' },
    });
    expect(claude.approvalsWaiting(claude.thread)).toEqual([]);
    const questions = [{ question: 'Which database?', header: 'Database', options: [], multiSelect: false }];
    const asked = claude.ask('AskUserQuestion', { questions }, { toolUseID: 'question-full' });
    await flush();
    expect(claude.answerQuestion('question-full', { 'Which database?': 'SQLite' })).toBe(true);
    expect(await asked.answer).toEqual({
      behavior: 'allow',
      updatedInput: { questions, answers: { 'Which database?': 'SQLite' } },
    });

    await claude.close();
    expect(await claude.ask('Bash', { command: 'ls' }).answer).toEqual({
      behavior: 'deny',
      message: 'This Kratos thread is not available',
    });
  });

  it('answers questions and elicitations, and defers the turn deadline while any are pending', async () => {
    const claude = await setup({ turnTimeoutMs: 50 });
    vi.useFakeTimers();
    await claude.start(A);
    await claude.sendMessage(claude.thread, input('B'), B);
    const questions = [
      {
        question: 'Which database?',
        header: 'Database',
        options: [
          { label: 'Postgres', description: 'Server' },
          { label: 'SQLite', description: 'File' },
        ],
        multiSelect: false,
      },
    ];

    const asked = claude.ask('AskUserQuestion', { questions }, { toolUseID: 'question-1' });
    await flush();
    expect(claude.events).toContainEqual({
      kind: 'question.requested',
      question: { requestId: 'question-1', threadId: claude.thread, turnId: A, questions },
    });
    expect(claude.questionsWaiting(claude.thread)).toHaveLength(1);
    expect(claude.approvalsWaiting(claude.thread)).toEqual([]);
    await vi.advanceTimersByTimeAsync(200);
    expect(claude.launch().interrupts).toBe(0);
    expect(claude.answerQuestion('question-1', { 'Which database?': 'SQLite' })).toBe(true);
    expect(await asked.answer).toEqual({
      behavior: 'allow',
      updatedInput: { questions, answers: { 'Which database?': 'SQLite' } },
    });
    expect(claude.answerQuestion('question-1', {})).toBe(false);

    const request = { serverName: 'github', message: 'Sign in', mode: 'url' as const, url: 'https://example.com' };
    const signIn = claude.elicit(request);
    await flush();
    const [elicitation] = claude.elicitationsWaiting(claude.thread);
    expect(elicitation).toMatchObject({ threadId: claude.thread, request });
    expect(claude.events).toContainEqual({ kind: 'elicitation.requested', elicitation });
    await vi.advanceTimersByTimeAsync(200);
    expect(claude.launch().interrupts).toBe(0);
    expect(claude.answerElicitation(elicitation?.requestId ?? '', { action: 'cancel' })).toBe(true);
    expect(await signIn.answer).toEqual({ action: 'cancel' });

    const form = claude.elicit({ serverName: 'jira', message: 'Project key', mode: 'form', requestedSchema: {} });
    await flush();
    form.abort();
    expect(await form.answer).toEqual({ action: 'cancel' });
    expect(claude.elicitationsWaiting(claude.thread)).toEqual([]);

    const idle = claude.waitUntilIdle(claude.thread).catch((error: unknown) => error);
    await vi.advanceTimersByTimeAsync(50);
    const message = 'Claude turn timed out after 50 ms';
    expect(claude.launch().interrupts).toBe(1);
    expect(claude.events).toContainEqual({ kind: 'turn.timedOut', threadId: claude.thread, turnId: A, message });
    await claude.finish([A], {
      subtype: 'error_during_execution',
      is_error: true,
      errors: [],
      terminal_reason: 'aborted_streaming',
    });
    expect(await idle).toMatchObject({ message });
    expect(claude.written()).toEqual([A]);
  });
});
