import { randomUUID } from 'node:crypto';
import { z } from 'zod';
import { connectCodex, CodexRequestError, CodexTimeoutError, type CodexOptions, type v2 } from '@kratos/codex';
import { codexInputSchema, type Store, type CodexMessage, type CodexThread } from '../store';
import { createCodexApprovals, type Approval, type Permission } from './codex-approvals';
import { createTurnChanges, type TurnChanges } from './turn-changes';

type UserInput = v2.UserInput;
export type CodexEvent =
  | ({ kind: 'message.delta' } & v2.AgentMessageDeltaNotification)
  | ({ kind: 'item.started' } & v2.ItemStartedNotification)
  | ({ kind: 'item.completed' } & v2.ItemCompletedNotification)
  | ({ kind: 'turn.started' } & v2.TurnStartedNotification)
  | ({ kind: 'turn.completed'; changes: TurnChanges } & v2.TurnCompletedNotification)
  | ({ kind: 'error' } & v2.ErrorNotification)
  | ({ kind: 'serverRequest.resolved' } & v2.ServerRequestResolvedNotification)
  | { kind: 'turn.startFailed'; threadId: string; message: string }
  | { kind: 'turn.timedOut'; threadId: string; turnId: string; message: string }
  | { kind: 'thread.closed'; threadId: string }
  | { kind: 'message.updated'; message: CodexMessage }
  | { kind: 'provider.exited'; exitCode: number | null }
  | { kind: 'approvals.changed'; threadId: string; approvals: Approval[] }
  | { kind: 'userInput.requested'; requestId: string | number; params: v2.ToolRequestUserInputParams }
  | { kind: 'permissions.requested'; requestId: string | number; params: v2.PermissionsRequestApprovalParams }
  | { kind: 'elicitation.requested'; requestId: string | number; params: v2.McpServerElicitationRequestParams };

export interface CodexProviderOptions extends CodexOptions {
  store: Store;
  turnTimeoutMs?: number;
}

interface ActiveTurn {
  kind: 'running' | 'stopping';
  turnId: string;
  stopPromise?: Promise<void>;
}
interface StartingTurn {
  kind: 'starting';
  messageId: string;
  startPromise: Promise<ActiveTurn | undefined>;
  completedTurns: Map<string, v2.Turn>;
  stopPromise?: Promise<void>;
}
type TurnState = { kind: 'idle' } | StartingTurn | ActiveTurn | { kind: 'unavailable'; error: Error };
interface ThreadState {
  settings: CodexThread;
  turn: TurnState;
  steering: Map<string, Promise<CodexMessage>>;
  changes: ReturnType<typeof createTurnChanges>;
  waiters: Set<{ resolve: () => void; reject: (error: Error) => void }>;
  timer?: NodeJS.Timeout;
  lastCompletedTurnId?: string;
}

const turnSchema = z.object({
  id: z.string().min(1),
  status: z.enum(['inProgress', 'completed', 'failed', 'interrupted']),
  error: z.object({ message: z.string() }).nullable().optional(),
});
const historyTurnSchema = turnSchema.extend({
  items: z.array(z.object({ type: z.string(), clientId: z.string().nullable().optional() })),
});
const threadSchema = z.object({
  id: z.string().min(1),
  cwd: z.string().min(1),
  turns: z.array(historyTurnSchema),
  status: z.object({ type: z.enum(['notLoaded', 'idle', 'active', 'systemError']) }),
});
const turnFailure = (turn: v2.Turn) => turn.error?.message ?? 'Codex turn failed';
const messageText = (error: unknown) => (error instanceof Error ? error.message : String(error));
const beforeExecution = (error: unknown) =>
  error instanceof CodexRequestError && [-32700, -32600, -32601, -32602].includes(error.code);

function permissions(
  permissions: Permission,
  readOnly: boolean,
): Required<Pick<v2.TurnStartParams, 'sandboxPolicy' | 'approvalPolicy' | 'approvalsReviewer'>> {
  if (readOnly)
    return {
      sandboxPolicy: { type: 'readOnly', networkAccess: false },
      approvalPolicy: 'never',
      approvalsReviewer: 'user',
    };
  if (permissions === 'fullAccess')
    return { sandboxPolicy: { type: 'dangerFullAccess' }, approvalPolicy: 'never', approvalsReviewer: 'user' };
  return {
    sandboxPolicy: {
      type: 'workspaceWrite',
      writableRoots: [],
      networkAccess: false,
      excludeTmpdirEnvVar: false,
      excludeSlashTmp: false,
    },
    approvalPolicy: 'on-request',
    approvalsReviewer: permissions === 'approveForMe' ? 'auto_review' : 'user',
  };
}

function threadPermissions(permission: Permission, readOnly: boolean) {
  const { approvalPolicy, approvalsReviewer } = permissions(permission, readOnly);
  const sandbox: v2.SandboxMode = readOnly
    ? 'read-only'
    : permission === 'fullAccess'
      ? 'danger-full-access'
      : 'workspace-write';
  return { sandbox, approvalPolicy, approvalsReviewer };
}

export async function createCodexProvider(options: CodexProviderOptions) {
  const { store, turnTimeoutMs = 600000, ...connectionOptions } = options;
  if (!Number.isFinite(turnTimeoutMs) || turnTimeoutMs <= 0) throw new Error('Turn timeout must be positive');
  const codex = await connectCodex({
    clientInfo: { name: 'kratos', title: 'Kratos', version: '0.1.0' },
    ...connectionOptions,
    capabilities: { ...connectionOptions.capabilities, experimentalApi: true, requestAttestation: false },
  });
  const threads = new Map<string, ThreadState>();
  const eventListeners = new Set<(event: CodexEvent) => unknown>();
  const submissions = new Map<string, Promise<CodexMessage>>();
  const resumes = new Map<string, Promise<string>>();
  let closed = false;
  let closePromise: Promise<void> | undefined;
  const { clearApprovals, ...approvals } = createCodexApprovals(
    codex,
    (id) => {
      const state = threads.get(id);
      return closed || state?.turn.kind === 'unavailable' || state?.settings.readOnly
        ? undefined
        : state?.settings.permissions;
    },
    (threadId) => emitEvent({ kind: 'approvals.changed', threadId, approvals: approvals.approvalsWaiting(threadId) }),
  );
  const userInputs = new Map<
    string | number,
    { params: v2.ToolRequestUserInputParams; resolve: (reply: v2.ToolRequestUserInputResponse) => void }
  >();
  const permissionRequests = new Map<
    string | number,
    { params: v2.PermissionsRequestApprovalParams; resolve: (reply: v2.PermissionsRequestApprovalResponse) => void }
  >();
  const elicitations = new Map<
    string | number,
    { params: v2.McpServerElicitationRequestParams; resolve: (reply: v2.McpServerElicitationRequestResponse) => void }
  >();

  function emitEvent(event: CodexEvent) {
    for (const listener of eventListeners) {
      try {
        const result = listener(structuredClone(event));
        if (result instanceof Promise)
          void result.catch((error: unknown) => console.error('Codex event listener threw', error));
      } catch (error) {
        console.error('Codex event listener threw', error);
      }
    }
  }

  function onEvent(listener: (event: CodexEvent) => unknown): () => void {
    eventListeners.add(listener);
    return () => {
      eventListeners.delete(listener);
    };
  }

  function requireThread(id: string): ThreadState {
    if (closed) throw new Error('Codex provider is closed');
    const state = threads.get(id);
    if (!state) throw new Error('Start or resume thread ' + id + ' before using it');
    if (state.turn.kind === 'unavailable') throw state.turn.error;
    return state;
  }

  function updateMessage(
    id: string,
    status: CodexMessage['status'],
    turnId: string | null = null,
    error: string | null = null,
  ) {
    const message = store.updateCodexMessage(id, status, turnId, error);
    emitEvent({ kind: 'message.updated', message });
    return message;
  }

  function saveThreadSettings(state: ThreadState, settings: CodexThread) {
    store.saveCodexThread(settings);
    state.settings = settings;
  }

  function setQueueError(state: ThreadState, error: string | null) {
    saveThreadSettings(state, { ...state.settings, pausedError: error });
  }

  function settleWaiters(state: ThreadState) {
    if (state.turn.kind !== 'idle' && state.turn.kind !== 'unavailable') return;
    if (state.steering.size) return;
    const error =
      state.turn.kind === 'unavailable'
        ? state.turn.error
        : state.settings.pausedError
          ? new Error(state.settings.pausedError)
          : undefined;
    for (const waiter of state.waiters) {
      if (error) waiter.reject(error);
      else waiter.resolve();
    }
    state.waiters.clear();
  }

  function clearRequests(threadId?: string, turnId?: string) {
    const matches = (params: { threadId: string; turnId: string | null }) =>
      (threadId === undefined || params.threadId === threadId) && (turnId === undefined || params.turnId === turnId);
    for (const [id, request] of userInputs)
      if (matches(request.params)) {
        userInputs.delete(id);
        request.resolve({ answers: {} });
      }
    for (const [id, request] of permissionRequests)
      if (matches(request.params)) {
        permissionRequests.delete(id);
        request.resolve({ permissions: {}, scope: 'turn' });
      }
    for (const [id, request] of elicitations)
      if (matches(request.params)) {
        elicitations.delete(id);
        request.resolve({ action: 'cancel', content: null, _meta: null });
      }
  }

  function pendingInput(threadId: string): boolean {
    return (
      approvals.approvalsWaiting(threadId).length > 0 ||
      [...userInputs.values()].some(({ params }) => params.threadId === threadId && params.isBlocking) ||
      [...permissionRequests.values(), ...elicitations.values()].some((request) => request.params.threadId === threadId)
    );
  }

  function unavailable(threadId: string, state: ThreadState, error: Error) {
    clearTimeout(state.timer);
    state.turn = { kind: 'unavailable', error };
    state.steering.clear();
    clearApprovals(threadId);
    clearRequests(threadId);
    try {
      for (const message of store.codexMessages(threadId))
        if (message.status === 'submitting') {
          updateMessage(message.id, 'unknown', message.turnId, error.message);
        }
    } catch (error) {
      console.error('Could not save Codex recovery state', error);
    } finally {
      settleWaiters(state);
    }
  }

  function failProvider(threadId: string, state: ThreadState, reason: unknown): Error {
    const error = reason instanceof Error ? reason : new Error(String(reason));
    unavailable(threadId, state, error);
    void close().catch((error: unknown) => console.error(error));
    return error;
  }

  function startTurnTimeout(threadId: string, state: ThreadState, turn: ActiveTurn) {
    state.timer = setTimeout(() => {
      if (state.turn !== turn) return;
      if (pendingInput(threadId)) {
        startTurnTimeout(threadId, state, turn);
        return;
      }
      void (async () => {
        const error = new CodexTimeoutError('turn', turnTimeoutMs);
        setQueueError(state, error.message);
        emitEvent({ kind: 'turn.timedOut', threadId, turnId: turn.turnId, message: error.message });
        try {
          await stopTurn(threadId);
        } catch {
          if (state.turn === turn) await close();
        }
      })().catch((error: unknown) => {
        failProvider(threadId, state, error);
      });
    }, turnTimeoutMs);
  }

  function completeTurn(threadId: string, turn: v2.Turn) {
    turnSchema.parse(turn);
    const state = threads.get(threadId);
    if (!state || state.turn.kind === 'unavailable') return;
    if (state.turn.kind === 'starting') {
      state.turn.completedTurns.set(turn.id, turn);
      return;
    }
    if (state.turn.kind === 'idle' || state.turn.turnId !== turn.id) return;
    clearTimeout(state.timer);
    clearApprovals(threadId, turn.id);
    clearRequests(threadId, turn.id);
    state.turn = { kind: 'idle' };
    try {
      if (turn.status === 'failed') setQueueError(state, turnFailure(turn));
    } catch (error) {
      failProvider(threadId, state, error);
      return;
    }
    void dispatchQueue(threadId).catch(() => undefined);
    settleWaiters(state);
  }

  async function findAcceptedTurn(threadId: string, messageId: string): Promise<v2.Turn | undefined> {
    const { thread } = await codex.request('thread/read', { threadId, includeTurns: true });
    threadSchema.extend({ id: z.literal(threadId) }).parse(thread);
    const turn = thread.turns.find((turn) =>
      turn.items.some((item) => item.type === 'userMessage' && item.clientId === messageId),
    );
    if (turn) turnSchema.parse(turn);
    return turn;
  }

  async function requestStart(request: v2.TurnStartParams) {
    try {
      return await codex.request('turn/start', request);
    } catch (error) {
      if (!beforeExecution(error)) throw error;
      return codex.request('turn/start', request);
    }
  }

  function finishStart(
    threadId: string,
    state: ThreadState,
    starting: StartingTurn,
    turn: v2.Turn,
  ): ActiveTurn | undefined {
    turnSchema.parse(turn);
    try {
      updateMessage(starting.messageId, 'accepted', turn.id);
    } catch (error) {
      throw failProvider(threadId, state, error);
    }
    if (state.turn !== starting) return undefined;
    const active: ActiveTurn = { kind: 'running', turnId: turn.id };
    if (starting.stopPromise) active.stopPromise = starting.stopPromise;
    state.turn = active;
    const completed = starting.completedTurns.get(turn.id);
    if (completed || turn.status !== 'inProgress') {
      completeTurn(threadId, completed ?? turn);
      if (!completed && state.lastCompletedTurnId !== turn.id) {
        state.lastCompletedTurnId = turn.id;
        emitEvent({ kind: 'turn.completed', threadId, turn, changes: state.changes.take() });
      }
      return undefined;
    }
    startTurnTimeout(threadId, state, active);
    return active;
  }

  function startTurn(threadId: string, message: CodexMessage): Promise<ActiveTurn | undefined> {
    const state = requireThread(threadId);
    const request: v2.TurnStartParams = {
      threadId,
      input: message.input,
      clientUserMessageId: message.id,
      model: state.settings.model,
      ...permissions(state.settings.permissions, state.settings.readOnly),
    };
    const submitting = store.updateCodexMessage(message.id, 'submitting');
    const starting: StartingTurn = {
      kind: 'starting',
      messageId: message.id,
      completedTurns: new Map(),
      startPromise: Promise.resolve().then(() => submit()),
    };
    state.turn = starting;
    state.changes.take();
    const submit = async (): Promise<ActiveTurn | undefined> => {
      try {
        if (closed) throw new Error('Codex provider is closed');
        const { turn } = await requestStart(request);
        return finishStart(threadId, state, starting, turn);
      } catch (error) {
        if (state.turn !== starting) throw error;
        const turn = beforeExecution(error)
          ? undefined
          : await findAcceptedTurn(threadId, message.id).catch(() => undefined);
        if (state.turn !== starting) throw error;
        if (turn) return finishStart(threadId, state, starting, turn);
        state.turn = { kind: 'idle' };
        const reason = messageText(error);
        try {
          updateMessage(message.id, beforeExecution(error) ? 'failed' : 'unknown', null, reason);
          setQueueError(state, reason);
        } catch (storageError) {
          throw failProvider(threadId, state, storageError);
        }
        emitEvent({ kind: 'turn.startFailed', threadId, message: reason });
        settleWaiters(state);
        if (error instanceof CodexTimeoutError) await close();
        throw error;
      }
    };
    emitEvent({ kind: 'message.updated', message: submitting });
    return starting.startPromise;
  }

  async function dispatchQueue(threadId: string): Promise<void> {
    const state = requireThread(threadId);
    if (state.turn.kind !== 'idle' || state.settings.pausedError || state.steering.size) return;
    try {
      const next = store.codexMessages(threadId).find((message) => !['accepted', 'cancelled'].includes(message.status));
      if (next?.status !== 'queued') return;
      await startTurn(threadId, next);
    } catch (error) {
      if (!state.settings.pausedError && threads.get(threadId)?.turn.kind !== 'unavailable') {
        const failure = failProvider(threadId, state, error);
        emitEvent({ kind: 'turn.startFailed', threadId, message: failure.message });
      }
      throw error;
    }
  }

  async function sendMessage(
    threadId: string,
    input: UserInput[],
    messageId: string = randomUUID(),
  ): Promise<CodexMessage> {
    requireThread(threadId);
    const message = store.addCodexMessage(messageId, threadId, codexInputSchema.parse(input));
    const existing = submissions.get(messageId);
    if (existing) return existing;
    if (message.status !== 'queued') return message;
    const sending = dispatchQueue(threadId).then(() => store.getCodexMessage(messageId) ?? message);
    submissions.set(messageId, sending);
    try {
      return await sending;
    } finally {
      submissions.delete(messageId);
    }
  }

  async function steerTurn(threadId: string, messageId: string): Promise<CodexMessage> {
    const state = requireThread(threadId);
    const message = store.getCodexMessage(messageId);
    if (!message || message.threadId !== threadId) throw new Error('Unknown queued message ' + messageId);
    const existing = state.steering.get(messageId);
    if (existing) return existing;
    if (message.status !== 'queued' || state.turn.kind !== 'running') return message;
    const turnId = state.turn.turnId;
    try {
      updateMessage(messageId, 'submitting', turnId);
    } catch (error) {
      throw failProvider(threadId, state, error);
    }
    const steering = (async () => {
      try {
        const result = await codex.request('turn/steer', {
          threadId,
          expectedTurnId: turnId,
          input: message.input,
          clientUserMessageId: messageId,
        });
        if (result.turnId !== turnId) throw new Error('Codex steering returned a different turn');
        return updateMessage(messageId, 'accepted', turnId);
      } catch (error) {
        if (state.turn.kind === 'unavailable') throw error;
        try {
          if (beforeExecution(error)) updateMessage(messageId, 'queued');
          else {
            const delivered = await findAcceptedTurn(threadId, messageId).catch(() => undefined);
            if (delivered) return updateMessage(messageId, 'accepted', delivered.id);
            updateMessage(messageId, 'unknown', turnId, messageText(error));
            setQueueError(state, messageText(error));
          }
        } catch (storageError) {
          throw failProvider(threadId, state, storageError);
        }
        throw error;
      } finally {
        state.steering.delete(messageId);
        void dispatchQueue(threadId).catch(() => undefined);
        settleWaiters(state);
      }
    })();
    state.steering.set(messageId, steering);
    return steering;
  }

  async function stopTurn(threadId: string) {
    const state = requireThread(threadId);
    const turn = state.turn;
    if (turn.kind === 'idle') return;
    if (turn.kind === 'unavailable') throw turn.error;
    if (turn.stopPromise) return turn.stopPromise;
    const interrupt = async () => {
      const activeTurn = turn.kind === 'starting' ? await turn.startPromise : turn;
      if (!activeTurn || state.turn !== activeTurn) return;
      activeTurn.kind = 'stopping';
      try {
        await codex.request('turn/interrupt', { threadId, turnId: activeTurn.turnId }, { timeoutMs: 5000 });
        if (state.turn === activeTurn) {
          clearTimeout(state.timer);
          state.timer = setTimeout(() => {
            if (state.turn === activeTurn) void close().catch((error: unknown) => console.error(error));
          }, 5000);
        }
      } catch (error) {
        if (state.turn === activeTurn) {
          activeTurn.kind = 'running';
          delete activeTurn.stopPromise;
        }
        throw error;
      }
    };
    turn.stopPromise = interrupt();
    return turn.stopPromise;
  }

  async function waitUntilIdle(threadId: string) {
    const state = requireThread(threadId);
    if (state.turn.kind === 'idle' && !state.steering.size) {
      if (state.settings.pausedError) throw new Error(state.settings.pausedError);
      return;
    }
    return new Promise<void>((resolve, reject) => state.waiters.add({ resolve, reject }));
  }

  function setThreadModel(threadId: string, model: string) {
    const state = requireThread(threadId);
    if (!model.trim()) throw new Error('Model must not be empty');
    saveThreadSettings(state, { ...state.settings, model });
  }

  function setThreadPermissions(threadId: string, permission: Permission) {
    const state = requireThread(threadId);
    if (state.turn.kind !== 'idle' || state.steering.size) throw new Error('Stop the turn before changing permissions');
    const permissions = z.enum(['askMe', 'approveForMe', 'fullAccess']).parse(permission);
    saveThreadSettings(state, { ...state.settings, permissions });
  }

  function setThreadReadOnly(threadId: string, readOnly: boolean) {
    const state = requireThread(threadId);
    if (state.turn.kind !== 'idle' || state.steering.size) throw new Error('Stop the turn before changing permissions');
    saveThreadSettings(state, { ...state.settings, readOnly });
  }

  function removeQueuedMessage(threadId: string, messageId: string): boolean {
    const state = requireThread(threadId);
    const message = store.getCodexMessage(messageId);
    if (!message || message.threadId !== threadId || !['queued', 'failed'].includes(message.status)) return false;
    updateMessage(messageId, 'cancelled');
    if (
      message.status === 'failed' &&
      !store.codexMessages(threadId).some((item) => ['failed', 'unknown'].includes(item.status))
    )
      setQueueError(state, null);
    void dispatchQueue(threadId).catch(() => undefined);
    return true;
  }

  async function retryMessage(threadId: string, messageId: string): Promise<CodexMessage> {
    const state = requireThread(threadId);
    const message = store.getCodexMessage(messageId);
    if (!message || message.threadId !== threadId || message.status !== 'failed')
      throw new Error('Only messages confirmed not to have executed can be retried');
    updateMessage(messageId, 'queued');
    setQueueError(state, null);
    await dispatchQueue(threadId);
    return store.getCodexMessage(messageId) ?? message;
  }

  async function discardMessage(threadId: string, messageId: string): Promise<CodexMessage> {
    const state = requireThread(threadId);
    const message = store.getCodexMessage(messageId);
    if (!message || message.threadId !== threadId || message.status !== 'unknown')
      throw new Error('Only messages with uncertain delivery can be discarded');
    const idle = state.turn;
    if (idle.kind !== 'idle' || state.steering.size) throw new Error('Stop the turn before discarding a message');
    const { thread } = await codex.request('thread/read', { threadId, includeTurns: true });
    threadSchema.extend({ id: z.literal(threadId) }).parse(thread);
    if (requireThread(threadId) !== state || state.turn !== idle || thread.status.type !== 'idle')
      throw new Error('Codex must be idle before discarding a message');
    const accepted = thread.turns.find((turn) =>
      turn.items.some((item) => item.type === 'userMessage' && item.clientId === messageId),
    );
    if (accepted) return updateMessage(messageId, 'accepted', accepted.id);
    // Discarding prevents a replay. It cannot undo work Codex may already have performed.
    return updateMessage(messageId, 'cancelled', message.turnId, message.error);
  }

  async function resumeQueue(threadId: string) {
    const state = requireThread(threadId);
    if (store.codexMessages(threadId).some((message) => ['failed', 'unknown', 'submitting'].includes(message.status)))
      throw new Error('Resolve undelivered messages before resuming the queue');
    setQueueError(state, null);
    await dispatchQueue(threadId);
  }

  function registerThread(settings: CodexThread): ThreadState {
    store.saveCodexThread(settings);
    const state: ThreadState = {
      settings,
      turn: { kind: 'idle' },
      steering: new Map(),
      changes: createTurnChanges(),
      waiters: new Set(),
    };
    threads.set(settings.id, state);
    return state;
  }

  async function startThread(
    folder: string,
    settings: { model?: string; permissions?: Permission; readOnly?: boolean } = {},
  ): Promise<string> {
    if (closed) throw new Error('Codex provider is closed');
    const permission = z.enum(['askMe', 'approveForMe', 'fullAccess']).parse(settings.permissions ?? 'askMe');
    const readOnly = settings.readOnly ?? false;
    const result = await codex.request('thread/start', {
      cwd: folder,
      ephemeral: false,
      ...(settings.model ? { model: settings.model } : {}),
      ...threadPermissions(permission, readOnly),
    });
    z.object({ thread: threadSchema, model: z.string().min(1) }).parse(result);
    if (closed) throw new Error('Codex provider is closed');
    registerThread({
      id: result.thread.id,
      folder: result.thread.cwd,
      model: result.model,
      permissions: permission,
      readOnly,
      pausedError: null,
    });
    return result.thread.id;
  }

  function reconcileMessages(threadId: string, turns: v2.Turn[]) {
    for (const message of store.codexMessages(threadId)) {
      if (!['unknown', 'submitting'].includes(message.status)) continue;
      const turn = turns.find((turn) =>
        turn.items.some((item) => item.type === 'userMessage' && item.clientId === message.id),
      );
      if (turn) updateMessage(message.id, 'accepted', turn.id);
      else updateMessage(message.id, 'unknown', message.turnId, message.error ?? 'Delivery was interrupted');
    }
  }

  function restoreTurn(threadId: string, state: ThreadState, thread: v2.Thread) {
    const unresolved = store.codexMessages(threadId).find((message) => ['unknown', 'failed'].includes(message.status));
    if (unresolved && !state.settings.pausedError)
      setQueueError(state, unresolved.error ?? 'Resolve undelivered messages before resuming the queue');
    const active = thread.turns.findLast((turn) => turn.status === 'inProgress');
    if (thread.status.type === 'active') {
      if (!active) {
        unavailable(threadId, state, new Error('Could not establish the native thread state'));
        return;
      }
      const turn: ActiveTurn = { kind: 'running', turnId: active.id };
      state.turn = turn;
      startTurnTimeout(threadId, state, turn);
      return;
    }
    if (['systemError', 'notLoaded'].includes(thread.status.type)) {
      unavailable(threadId, state, new Error('Codex thread has a system error'));
      return;
    }
    const last = thread.turns.at(-1);
    switch (last?.status) {
      case 'failed':
        setQueueError(state, turnFailure(last));
        return;
      case 'inProgress':
        setQueueError(state, 'Last turn was interrupted before completion');
        return;
    }
  }

  async function resumeThread(threadId: string): Promise<string> {
    if (closed) throw new Error('Codex provider is closed');
    const current = threads.get(threadId);
    if (current && current.turn.kind !== 'unavailable') return threadId;
    const existing = resumes.get(threadId);
    if (existing) return existing;
    const resuming = (async () => {
      const saved = store.getCodexThread(threadId);
      const permission = saved?.permissions ?? 'askMe';
      const readOnly = saved?.readOnly ?? false;
      const result = await codex.request('thread/resume', {
        threadId,
        ...(saved ? { cwd: saved.folder, model: saved.model } : {}),
        ...threadPermissions(permission, readOnly),
      });
      z.object({
        thread: threadSchema.extend({ id: z.literal(threadId) }),
        model: z.string().min(1),
      }).parse(result);
      if (closed) throw new Error('Codex provider is closed');
      const state = registerThread({
        id: threadId,
        folder: result.thread.cwd,
        model: result.model,
        permissions: permission,
        readOnly,
        pausedError: saved?.pausedError ?? null,
      });
      try {
        reconcileMessages(threadId, result.thread.turns);
        restoreTurn(threadId, state, result.thread);
      } catch (error) {
        throw failProvider(threadId, state, error);
      }
      void dispatchQueue(threadId).catch(() => undefined);
      return threadId;
    })();
    resumes.set(threadId, resuming);
    try {
      return await resuming;
    } finally {
      resumes.delete(threadId);
    }
  }

  async function providerModels() {
    const models = new Map<string, { id: string; details: string; name: string }>();
    let cursor: string | null = null;
    const cursors = new Set<string>();
    do {
      const result: v2.ModelListResponse = await codex.request('model/list', {
        cursor,
        limit: null,
        includeHidden: null,
      });
      z.object({
        data: z.array(z.object({ model: z.string().min(1), displayName: z.string(), description: z.string() })),
        nextCursor: z.string().nullable(),
      }).parse(result);
      for (const model of result.data)
        models.set(model.model, { id: model.model, name: model.displayName, details: model.description });
      cursor = result.nextCursor;
      if (cursor && cursors.has(cursor)) throw new Error('Codex repeated a model page');
      if (cursor) cursors.add(cursor);
    } while (cursor);
    return models;
  }

  async function findSkills(folder: string) {
    const result = await codex.request('skills/list', { cwds: [folder] });
    return new Map(
      result.data.flatMap((entry) =>
        entry.skills.map(
          (skill) => [skill.path, { name: skill.name, details: skill.description, path: skill.path }] as const,
        ),
      ),
    );
  }

  async function findPlugins(folder: string) {
    const result = await codex.request('plugin/installed', { cwds: [folder] });
    return new Map(
      result.marketplaces.flatMap((marketplace) =>
        marketplace.plugins
          .filter((plugin) => plugin.installed)
          .map(
            (plugin) =>
              [
                plugin.id,
                {
                  name: plugin.name,
                  marketplace: marketplace.name,
                  enabled: plugin.enabled,
                  interface: plugin.interface,
                },
              ] as const,
          ),
      ),
    );
  }

  async function isSignedIn() {
    return (await codex.request('account/read', {})).account !== null;
  }

  async function close() {
    if (closePromise) return closePromise;
    closed = true;
    closePromise = Promise.resolve().then(async () => {
      try {
        const error = new Error('Codex provider closed');
        for (const [id, state] of threads) unavailable(id, state, error);
        clearRequests();
      } finally {
        await codex.close();
      }
    });
    return closePromise;
  }

  codex.onNotification('item/agentMessage/delta', (params) => emitEvent({ ...params, kind: 'message.delta' }));
  codex.onNotification('item/started', (params) => emitEvent({ ...params, kind: 'item.started' }));
  codex.onNotification('item/completed', (params) => {
    const changes = threads.get(params.threadId)?.changes;
    const { item } = params;
    if (item.type === 'fileChange' && item.status === 'completed')
      for (const { path } of item.changes) changes?.edited(path);
    else if (item.type === 'commandExecution' && (item.status === 'completed' || item.status === 'failed'))
      changes?.ran();
    emitEvent({ ...params, kind: 'item.completed' });
  });
  codex.onNotification('turn/started', (params) => emitEvent({ ...params, kind: 'turn.started' }));
  codex.onNotification('turn/completed', (params) => {
    completeTurn(params.threadId, params.turn);
    const state = threads.get(params.threadId);
    if (state?.lastCompletedTurnId === params.turn.id) return;
    if (state) state.lastCompletedTurnId = params.turn.id;
    emitEvent({ ...params, kind: 'turn.completed', changes: state?.changes.take() ?? { files: [], checked: true } });
  });
  codex.onNotification('error', (params) => emitEvent({ ...params, kind: 'error' }));
  codex.onNotification('thread/closed', ({ threadId }) => {
    const state = threads.get(threadId);
    if (state) unavailable(threadId, state, new Error('Codex thread closed'));
    emitEvent({ kind: 'thread.closed', threadId });
  });
  codex.onExit((exitCode) => {
    closed = true;
    for (const [id, state] of threads) unavailable(id, state, new Error('Codex exited'));
    clearRequests();
    emitEvent({ kind: 'provider.exited', exitCode });
  });

  codex.onServerRequest('item/tool/requestUserInput', (params, requestId) => {
    if (closed || !threads.has(params.threadId) || threads.get(params.threadId)?.turn.kind === 'unavailable')
      return { answers: {} };
    return new Promise<v2.ToolRequestUserInputResponse>((resolve) => {
      userInputs.set(requestId, { params, resolve });
      emitEvent({ kind: 'userInput.requested', requestId, params });
    });
  });
  codex.onServerRequest('item/permissions/requestApproval', (params, requestId) => {
    const state = threads.get(params.threadId);
    if (closed || !state || state.turn.kind === 'unavailable' || state.settings.readOnly)
      return { permissions: {}, scope: 'turn' };
    return new Promise<v2.PermissionsRequestApprovalResponse>((resolve) => {
      permissionRequests.set(requestId, { params, resolve });
      emitEvent({ kind: 'permissions.requested', requestId, params });
    });
  });
  codex.onServerRequest('mcpServer/elicitation/request', (params, requestId) => {
    if (closed || !threads.has(params.threadId) || threads.get(params.threadId)?.turn.kind === 'unavailable')
      return { action: 'cancel', content: null, _meta: null };
    return new Promise<v2.McpServerElicitationRequestResponse>((resolve) => {
      elicitations.set(requestId, { params, resolve });
      emitEvent({ kind: 'elicitation.requested', requestId, params });
    });
  });
  codex.onNotification('serverRequest/resolved', (params) => {
    const { requestId } = params;
    const question = userInputs.get(requestId);
    if (question) {
      userInputs.delete(requestId);
      question.resolve({ answers: {} });
    }
    const permission = permissionRequests.get(requestId);
    if (permission) {
      permissionRequests.delete(requestId);
      permission.resolve({ permissions: {}, scope: 'turn' });
    }
    const elicitation = elicitations.get(requestId);
    if (elicitation) {
      elicitations.delete(requestId);
      elicitation.resolve({ action: 'cancel', content: null, _meta: null });
    }
    emitEvent({ ...params, kind: 'serverRequest.resolved' });
  });

  function answerUserInput(requestId: string | number, response: v2.ToolRequestUserInputResponse): boolean {
    if (closed) return false;
    const request = userInputs.get(requestId);
    if (!request) return false;
    userInputs.delete(requestId);
    request.resolve(response);
    return true;
  }

  function decidePermissions(requestId: string | number, response: v2.PermissionsRequestApprovalResponse): boolean {
    if (closed) return false;
    const request = permissionRequests.get(requestId);
    if (!request) return false;
    permissionRequests.delete(requestId);
    request.resolve(response);
    return true;
  }

  function answerElicitation(requestId: string | number, response: v2.McpServerElicitationRequestResponse): boolean {
    if (closed) return false;
    const request = elicitations.get(requestId);
    if (!request) return false;
    elicitations.delete(requestId);
    request.resolve(response);
    return true;
  }

  return {
    startThread,
    resumeThread,
    sendMessage,
    steerTurn,
    stopTurn,
    waitUntilIdle,
    retryMessage,
    discardMessage,
    removeQueuedMessage,
    resumeQueue,
    setThreadModel,
    setThreadPermissions,
    setThreadReadOnly,
    providerModels,
    findSkills,
    findPlugins,
    isSignedIn,
    onEvent,
    close,
    messages: (threadId: string) => store.codexMessages(threadId),
    answerUserInput,
    decidePermissions,
    answerElicitation,
    userInputsWaiting: (threadId: string) =>
      [...userInputs]
        .filter(([, request]) => request.params.threadId === threadId)
        .map(([requestId, request]) => structuredClone({ requestId, params: request.params })),
    permissionsWaiting: (threadId: string) =>
      [...permissionRequests]
        .filter(([, request]) => request.params.threadId === threadId)
        .map(([requestId, request]) => structuredClone({ requestId, params: request.params })),
    elicitationsWaiting: (threadId: string) =>
      [...elicitations]
        .filter(([, request]) => request.params.threadId === threadId)
        .map(([requestId, request]) => structuredClone({ requestId, params: request.params })),
    ...approvals,
    decideCommand: (requestId: string | number, decision: v2.CommandExecutionApprovalDecision) =>
      !closed && approvals.decideCommand(requestId, decision),
    decideFile: (requestId: string | number, decision: v2.FileChangeApprovalDecision) =>
      !closed && approvals.decideFile(requestId, decision),
  };
}

export type CodexProvider = Awaited<ReturnType<typeof createCodexProvider>>;
