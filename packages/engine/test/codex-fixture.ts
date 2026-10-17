import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { afterEach, vi } from 'vitest';
import {
  connectCodex,
  CodexRequestError,
  CodexTimeoutError,
  type CollaborationMode,
  type NotificationMethod,
  type NotificationParams,
  type ServerRequestMethod,
  type ServerRequestParams,
  type v2,
} from '@kratos/codex';
import type { CodexEvent, CodexProvider, CodexProviderOptions } from '../src/providers/codex';
import { Store } from '../src/store';

interface TurnRequest {
  method: string;
  params: {
    threadId: string;
    turnId?: string;
    input?: v2.UserInput[];
    clientUserMessageId?: string;
    sandboxPolicy?: v2.SandboxPolicy;
    approvalPolicy?: v2.AskForApproval;
    approvalsReviewer?: v2.ApprovalsReviewer;
    collaborationMode?: CollaborationMode;
  };
}
interface FakeControls {
  request(method: string, params: unknown): Promise<unknown>;
}
const contexts: { provider: CodexProvider; store: Store; home: string }[] = [];

afterEach(async () => {
  vi.useRealTimers();
  for (const { provider, store, home } of contexts.splice(0)) {
    await provider.close();
    store.close();
    if (dirname(home) !== tmpdir()) throw new Error('Invalid test directory');
    rmSync(home, { recursive: true, force: true });
  }
  vi.doUnmock('@kratos/codex');
  vi.restoreAllMocks();
});

export function input(text: string): v2.UserInput[] {
  return [{ type: 'text', text, text_elements: [] }];
}
export function turn(id: string, status: v2.Turn['status'] = 'inProgress', error: v2.TurnError | null = null): v2.Turn {
  return {
    id,
    status,
    error,
    items: [],
    itemsView: 'full',
    startedAt: 100,
    completedAt: status === 'inProgress' ? null : 102,
    durationMs: status === 'inProgress' ? null : 2000,
  };
}

export async function setup(options: Partial<Omit<CodexProviderOptions, 'store'>> = {}) {
  const home = mkdtempSync(join(tmpdir(), 'kratos-provider-'));
  const store = new Store(home);
  const client = await connectCodex({
    command: process.execPath,
    args: [fileURLToPath(new URL('../../codex/test/fake-codex.mjs', import.meta.url))],
    ...options,
  });
  const controls = client as unknown as FakeControls;
  const settle = async (params: unknown) => {
    await controls.request('test/echo', {});
    return controls.request('test/settleTurn', params);
  };
  vi.resetModules();
  vi.doMock('@kratos/codex', () => ({
    connectCodex: () => Promise.resolve(client),
    CodexRequestError,
    CodexTimeoutError,
  }));
  const { createCodexProvider } = await import('../src/providers/codex');
  const provider = await createCodexProvider({ store, ...options });
  const context = { provider, store, home };
  contexts.push(context);
  await provider.startThread('C:\\project-A');
  await provider.startThread('C:\\project-B');
  const events: CodexEvent[] = [];
  provider.onEvent((event) => {
    events.push(event);
  });
  return {
    ...provider,
    provider,
    restart: async () => {
      await context.provider.close();
      const nextClient = await connectCodex({
        command: process.execPath,
        args: [fileURLToPath(new URL('../../codex/test/fake-codex.mjs', import.meta.url))],
        ...options,
      });
      const nextControls = nextClient as unknown as FakeControls;
      vi.resetModules();
      vi.doMock('@kratos/codex', () => ({
        connectCodex: () => Promise.resolve(nextClient),
        CodexRequestError,
        CodexTimeoutError,
      }));
      const { createCodexProvider } = await import('../src/providers/codex');
      const nextProvider = await createCodexProvider({ store, ...options });
      context.provider = nextProvider;
      return {
        provider: nextProvider,
        control: (method: string, params: unknown) => nextControls.request(method, params),
        start: async (id: string) => {
          await nextControls.request('test/echo', {});
          return nextControls.request('test/settleTurn', {
            method: 'turn/start',
            threadId: 'thread-A',
            result: { turn: turn(id) },
          });
        },
      };
    },
    store,
    home,
    events,
    emit: <M extends NotificationMethod>(method: M, params: NotificationParams<M>) =>
      controls.request('test/emit', { method, params }),
    ask: <M extends ServerRequestMethod>(method: M, requestId: string | number, params: ServerRequestParams<M>) =>
      controls.request('test/ask', { method, requestId, params }),
    requests: async (method = 'turn/start') => {
      const requests = (await controls.request('test/turnRequests', {})) as TurnRequest[];
      return requests.filter((request) => request.method === method).map((request) => request.params);
    },
    settleStart: (result?: { turn: v2.Turn }, error?: string, code?: number) =>
      settle({ method: 'turn/start', threadId: 'thread-A', result, error, code }),
    start: (id: string, threadId = 'thread-A', status: v2.Turn['status'] = 'inProgress') =>
      settle({ method: 'turn/start', threadId, result: { turn: turn(id, status) } }),
    interrupt: (error?: string, threadId = 'thread-A') =>
      settle({ method: 'turn/interrupt', threadId, result: {}, error }),
    steer: (id: string, error?: string, code?: number) =>
      settle({
        method: 'turn/steer',
        threadId: 'thread-A',
        result: { turnId: id },
        error,
        code,
      }),
    complete: (id: string, threadId = 'thread-A', status: v2.Turn['status'] = 'interrupted') =>
      controls.request('test/emit', { method: 'turn/completed', params: { threadId, turn: turn(id, status) } }),
    failStart: (code = -32000) =>
      settle({
        method: 'turn/start',
        threadId: 'thread-A',
        error: 'Start rejected',
        code,
      }),
    crash: () => controls.request('test/exit', { code: 3 }).catch(() => undefined),
    flush: () => controls.request('test/echo', {}),
    control: (method: string, params: unknown) => controls.request(method, params),
    answers: () =>
      new Promise<unknown>((resolve) => {
        const stop = client.onNotification('test/answeredWithId' as NotificationMethod, (value) => {
          stop();
          resolve(value);
        });
      }),
  };
}
