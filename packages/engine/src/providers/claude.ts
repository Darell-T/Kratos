import { randomUUID } from 'node:crypto';
import { homedir } from 'node:os';
import { z } from 'zod';
import type { SDKMessage, SDKResultMessage } from '@anthropic-ai/claude-agent-sdk';
import type { ClaudeInput, ClaudeMessage, ClaudeThread, Store } from '../store';
import { createClaudeApprovals, type ClaudeRequestEvent, type Permission } from './claude-approvals';
import {
  deliveredMessageIds,
  describeClaude,
  openClaudeSession,
  type ClaudeSession,
  type ClaudeSessionEvent,
} from './claude-session';
import { createTurnChanges, type TurnChanges } from './turn-changes';

export type ClaudeEvent =
  | { kind: 'turn.started'; threadId: string; turnId: string; messageId: string }
  | {
      kind: 'turn.completed';
      threadId: string;
      turnId: string;
      messageIds: string[];
      outcome: 'completed' | 'interrupted' | 'failed';
      error: string | null;
      changes: TurnChanges;
      result: SDKResultMessage;
    }
  | { kind: 'turn.startFailed'; threadId: string; message: string }
  | { kind: 'turn.timedOut'; threadId: string; turnId: string; message: string }
  | { kind: 'output'; threadId: string; turnId: string | null; message: SDKMessage }
  | { kind: 'session.exited'; threadId: string; message: string; changes: TurnChanges }
  | { kind: 'message.updated'; message: ClaudeMessage }
  | ClaudeRequestEvent;

export interface ClaudeProviderOptions {
  store: Store;
  turnTimeoutMs?: number;
}

interface ActiveTurn {
  kind: 'running' | 'stopping';
  turnId: string;
  // Messages written to the session that no result has reported yet. The thread is busy while any remain.
  pending: Set<string>;
  opening?: Promise<ClaudeSession>;
  stopPromise?: Promise<void>;
}
type TurnState = { kind: 'idle' } | ActiveTurn | { kind: 'unavailable'; error: Error };
interface ThreadState {
  settings: ClaudeThread;
  turn: TurnState;
  session?: ClaudeSession;
  stale: boolean;
  changes: ReturnType<typeof createTurnChanges>;
  waiters: Set<{ resolve: () => void; reject: (error: Error) => void }>;
  timer?: NodeJS.Timeout;
}
type ResultEvent = Extract<ClaudeSessionEvent, { kind: 'result' }>;

const permissionSchema = z.enum(['askMe', 'approveForMe', 'fullAccess']);
const messageText = (error: unknown) => (error instanceof Error ? error.message : String(error));
const isActive = (turn: TurnState): turn is ActiveTurn => turn.kind === 'running' || turn.kind === 'stopping';
const activeTurnId = ({ turn }: { turn: TurnState }) => (isActive(turn) ? turn.turnId : null);

export function createClaudeProvider(options: ClaudeProviderOptions) {
  const { store, turnTimeoutMs = 600000 } = options;
  if (!Number.isFinite(turnTimeoutMs) || turnTimeoutMs <= 0) throw new Error('Turn timeout must be positive');
  const threads = new Map<string, ThreadState>();
  const eventListeners = new Set<(event: ClaudeEvent) => unknown>();
  const submissions = new Map<string, Promise<ClaudeMessage>>();
  const deliveries = new Map<string, { resolve: (message: ClaudeMessage) => void; reject: (error: Error) => void }>();
  const resumes = new Map<string, Promise<string>>();
  let closed = false;
  const { canUseTool, onElicitation, clearRequests, pendingInput, ...requests } = createClaudeApprovals((threadId) => {
    const state = threads.get(threadId);
    if (closed || !state || state.turn.kind === 'unavailable') return undefined;
    return {
      permissions: state.settings.permissions,
      plan: state.settings.plan,
      turnId: isActive(state.turn) ? state.turn.turnId : null,
    };
  }, emitEvent);

  function emitEvent(event: ClaudeEvent) {
    for (const listener of eventListeners) {
      try {
        const result = listener(structuredClone(event));
        if (result instanceof Promise)
          void result.catch((error: unknown) => console.error('Claude event listener threw', error));
      } catch (error) {
        console.error('Claude event listener threw', error);
      }
    }
  }

  function onEvent(listener: (event: ClaudeEvent) => unknown): () => void {
    eventListeners.add(listener);
    return () => {
      eventListeners.delete(listener);
    };
  }

  function requireThread(id: string): ThreadState {
    if (closed) throw new Error('Claude provider is closed');
    const state = threads.get(id);
    if (!state) throw new Error('Start or resume thread ' + id + ' before using it');
    if (state.turn.kind === 'unavailable') throw state.turn.error;
    return state;
  }

  function updateMessage(
    id: string,
    status: ClaudeMessage['status'],
    turnId: string | null = null,
    error: string | null = null,
  ) {
    const message = store.updateClaudeMessage(id, status, turnId, error);
    emitEvent({ kind: 'message.updated', message });
    const delivery = deliveries.get(id);
    if (delivery && ['accepted', 'failed', 'unknown'].includes(status)) {
      deliveries.delete(id);
      if (status === 'accepted') delivery.resolve(message);
      else delivery.reject(new Error(message.error ?? 'Claude did not accept the message'));
    }
    return message;
  }

  function saveThreadSettings(state: ThreadState, settings: ClaudeThread) {
    store.saveClaudeThread(settings);
    state.settings = settings;
  }

  function setQueueError(state: ThreadState, error: string | null) {
    saveThreadSettings(state, { ...state.settings, pausedError: error });
  }

  function settleWaiters(state: ThreadState) {
    if (isActive(state.turn)) return;
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

  function closeSession(state: ThreadState) {
    const session = state.session;
    delete state.session;
    state.stale = false;
    session?.close();
  }

  function unavailable(threadId: string, state: ThreadState, error: Error) {
    clearTimeout(state.timer);
    closeSession(state);
    state.turn = { kind: 'unavailable', error };
    clearRequests(threadId);
    try {
      for (const message of store.claudeMessages(threadId))
        if (message.status === 'submitting') updateMessage(message.id, 'unknown', message.turnId, error.message);
    } catch (error) {
      console.error('Could not save Claude recovery state', error);
    } finally {
      settleWaiters(state);
    }
  }

  function failProvider(threadId: string, state: ThreadState, reason: unknown): Error {
    const error = reason instanceof Error ? reason : new Error(String(reason));
    unavailable(threadId, state, error);
    void close();
    return error;
  }

  // A lost Claude Code process affects only its own thread, which reopens the session on its next turn.
  function retire(threadId: string, state: ThreadState, error: Error) {
    const initialized = state.session?.initialized ?? false;
    const changes = state.changes.take();
    closeSession(state);
    clearTimeout(state.timer);
    clearRequests(threadId);
    state.turn = { kind: 'idle' };
    void (async () => {
      saveThreadSettings(state, {
        ...state.settings,
        pausedError: state.settings.pausedError ?? error.message,
        runningTurn: null,
      });
      const delivered = await deliveredMessageIds(threadId, state.settings.folder).catch(() => new Set<string>());
      for (const message of store.claudeMessages(threadId)) {
        if (message.status !== 'submitting') continue;
        if (delivered.has(message.id)) updateMessage(message.id, 'accepted', message.turnId);
        else updateMessage(message.id, initialized ? 'unknown' : 'failed', message.turnId, error.message);
      }
      settleWaiters(state);
      emitEvent({ kind: 'session.exited', threadId, message: error.message, changes });
    })().catch((reason: unknown) => failProvider(threadId, state, reason));
  }

  function startTurnTimeout(threadId: string, state: ThreadState, turn: ActiveTurn) {
    state.timer = setTimeout(() => {
      if (state.turn !== turn) return;
      if (pendingInput(threadId)) {
        startTurnTimeout(threadId, state, turn);
        return;
      }
      const error = new Error(`Claude turn timed out after ${turnTimeoutMs} ms`);
      try {
        setQueueError(state, error.message);
      } catch (storageError) {
        failProvider(threadId, state, storageError);
        return;
      }
      emitEvent({ kind: 'turn.timedOut', threadId, turnId: turn.turnId, message: error.message });
      stopTurn(threadId).catch(() => {
        if (state.turn === turn) retire(threadId, state, error);
      });
    }, turnTimeoutMs);
  }

  function finishTurn(threadId: string, state: ThreadState) {
    clearTimeout(state.timer);
    clearRequests(threadId);
    state.turn = { kind: 'idle' };
    saveThreadSettings(state, { ...state.settings, runningTurn: null });
    if (state.stale) closeSession(state);
    void dispatchQueue(threadId).catch(() => undefined);
    settleWaiters(state);
  }

  function continueTurn(threadId: string, state: ThreadState, turn: ActiveTurn) {
    if (turn.kind !== 'stopping') return;
    // The interrupted run ended. Steered input that Claude Code had already queued now runs, and can be stopped again.
    turn.kind = 'running';
    delete turn.stopPromise;
    clearTimeout(state.timer);
    startTurnTimeout(threadId, state, turn);
  }

  function recordResult(threadId: string, state: ThreadState, turn: ActiveTurn, event: ResultEvent) {
    for (const id of event.messageIds) {
      if (turn.pending.has(id) && store.getClaudeMessage(id)?.status === 'submitting')
        updateMessage(id, 'accepted', turn.turnId);
      turn.pending.delete(id);
    }
    if (event.outcome === 'failed') setQueueError(state, event.error);
    emitEvent({
      kind: 'turn.completed',
      threadId,
      turnId: turn.turnId,
      messageIds: event.messageIds,
      outcome: event.outcome,
      error: event.error,
      changes: state.changes.take(),
      result: event.result,
    });
    if (turn.pending.size) continueTurn(threadId, state, turn);
    else finishTurn(threadId, state);
  }

  function completeTurn(threadId: string, state: ThreadState, event: ResultEvent) {
    const turn = state.turn;
    if (isActive(turn) && event.messageIds.some((id) => turn.pending.has(id)))
      recordResult(threadId, state, turn, event);
    else emitEvent({ kind: 'output', threadId, turnId: activeTurnId(state), message: event.result });
  }

  function exited(threadId: string, state: ThreadState, error: Error) {
    if (isActive(state.turn)) {
      retire(threadId, state, error);
      return;
    }
    closeSession(state);
    emitEvent({ kind: 'session.exited', threadId, message: error.message, changes: state.changes.take() });
  }

  function acceptMessage(state: ThreadState, messageId: string) {
    const turn = state.turn;
    if (isActive(turn) && turn.pending.has(messageId)) updateMessage(messageId, 'accepted', turn.turnId);
  }

  function handleSessionEvent(threadId: string, state: ThreadState, session: ClaudeSession, event: ClaudeSessionEvent) {
    if (state.session !== session) return;
    try {
      switch (event.kind) {
        case 'initialized':
          if (state.settings.model === null) saveThreadSettings(state, { ...state.settings, model: event.model });
          return;
        case 'accepted':
          acceptMessage(state, event.messageId);
          return;
        case 'output':
          emitEvent({ kind: 'output', threadId, turnId: activeTurnId(state), message: event.message });
          return;
        case 'result':
          completeTurn(threadId, state, event);
          return;
        case 'exited':
          exited(threadId, state, event.error);
      }
    } catch (error) {
      failProvider(threadId, state, error);
    }
  }

  async function openSession(
    threadId: string,
    state: ThreadState,
    turn: ActiveTurn,
    message: ClaudeMessage,
  ): Promise<ClaudeSession> {
    if (state.session) return state.session;
    let session: ClaudeSession;
    try {
      session = await openClaudeSession(
        {
          threadId,
          folder: state.settings.folder,
          model: state.settings.model,
          permissions: state.settings.permissions,
          plan: state.settings.plan,
          canUseTool: canUseTool(threadId),
          onElicitation: onElicitation(threadId),
          onToolUse: (use) => (use.kind === 'edit' ? state.changes.edited(use.path) : state.changes.ran()),
        },
        (event) => handleSessionEvent(threadId, state, session, event),
      );
    } catch (error) {
      if (state.turn === turn) {
        const reason = messageText(error);
        state.turn = { kind: 'idle' };
        updateMessage(message.id, 'failed', null, reason);
        setQueueError(state, reason);
        emitEvent({ kind: 'turn.startFailed', threadId, message: reason });
        settleWaiters(state);
      }
      throw error;
    }
    if (state.turn !== turn) {
      session.close();
      throw new Error('Claude provider is closed');
    }
    state.session = session;
    return session;
  }

  function deliver(turn: ActiveTurn, session: ClaudeSession, message: ClaudeMessage) {
    const submitting = updateMessage(message.id, 'submitting', turn.turnId);
    turn.pending.add(message.id);
    const delivered = new Promise<ClaudeMessage>((resolve, reject) => deliveries.set(message.id, { resolve, reject }));
    session.submit(submitting);
    return delivered;
  }

  async function startTurn(threadId: string, state: ThreadState, message: ClaudeMessage): Promise<ClaudeMessage> {
    const turn: ActiveTurn = { kind: 'running', turnId: message.id, pending: new Set([message.id]) };
    state.turn = turn;
    state.changes.take();
    turn.opening = openSession(threadId, state, turn, message);
    const session = await turn.opening;
    delete turn.opening;
    if (state.turn !== turn) throw new Error('Claude provider is closed');
    saveThreadSettings(state, { ...state.settings, runningTurn: turn.turnId });
    const delivered = deliver(turn, session, message);
    startTurnTimeout(threadId, state, turn);
    emitEvent({ kind: 'turn.started', threadId, turnId: turn.turnId, messageId: message.id });
    return delivered;
  }

  async function dispatchQueue(threadId: string): Promise<void> {
    const state = requireThread(threadId);
    if (state.turn.kind !== 'idle' || state.settings.pausedError) return;
    try {
      const next = store
        .claudeMessages(threadId)
        .find((message) => !['accepted', 'cancelled'].includes(message.status));
      if (next?.status !== 'queued') return;
      await startTurn(threadId, state, next);
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
    input: ClaudeInput,
    messageId: string = randomUUID(),
  ): Promise<ClaudeMessage> {
    requireThread(threadId);
    const message = store.addClaudeMessage(messageId, threadId, input);
    const existing = submissions.get(message.id);
    if (existing) return existing;
    if (message.status !== 'queued') return message;
    const sending = dispatchQueue(threadId).then(() => store.getClaudeMessage(message.id) ?? message);
    submissions.set(message.id, sending);
    try {
      return await sending;
    } finally {
      submissions.delete(message.id);
    }
  }

  async function steerTurn(threadId: string, messageId: string): Promise<ClaudeMessage> {
    const state = requireThread(threadId);
    const message = store.getClaudeMessage(messageId);
    if (!message || message.threadId !== threadId) throw new Error('Unknown queued message ' + messageId);
    const existing = submissions.get(message.id);
    if (existing) return existing;
    const { turn, session } = state;
    if (message.status !== 'queued' || turn.kind !== 'running' || turn.opening || !session) return message;
    let steering: Promise<ClaudeMessage>;
    try {
      steering = deliver(turn, session, message);
    } catch (error) {
      throw failProvider(threadId, state, error);
    }
    submissions.set(message.id, steering);
    try {
      return await steering;
    } finally {
      submissions.delete(message.id);
    }
  }

  async function interrupt(threadId: string, state: ThreadState, turn: ActiveTurn) {
    const session = turn.opening ? await turn.opening : state.session;
    if (state.turn !== turn || !session) return;
    turn.kind = 'stopping';
    clearTimeout(state.timer);
    state.timer = setTimeout(() => {
      if (state.turn === turn)
        retire(threadId, state, new Error('Claude did not stop the turn; its process was closed'));
    }, 5000);
    try {
      await session.interrupt();
    } catch (error) {
      if (state.turn === turn) {
        turn.kind = 'running';
        delete turn.stopPromise;
        clearTimeout(state.timer);
        startTurnTimeout(threadId, state, turn);
      }
      throw error;
    }
  }

  async function stopTurn(threadId: string) {
    const state = requireThread(threadId);
    const turn = state.turn;
    if (!isActive(turn)) return;
    turn.stopPromise ??= interrupt(threadId, state, turn);
    return turn.stopPromise;
  }

  async function waitUntilIdle(threadId: string) {
    const state = requireThread(threadId);
    if (!isActive(state.turn)) {
      if (state.settings.pausedError) throw new Error(state.settings.pausedError);
      return;
    }
    return new Promise<void>((resolve, reject) => state.waiters.add({ resolve, reject }));
  }

  function setThreadModel(threadId: string, model: string) {
    const state = requireThread(threadId);
    if (!model.trim()) throw new Error('Model must not be empty');
    saveThreadSettings(state, { ...state.settings, model });
    if (isActive(state.turn)) state.stale = true;
    else closeSession(state);
  }

  function setThreadPermissions(threadId: string, permission: Permission) {
    const state = requireThread(threadId);
    if (isActive(state.turn)) throw new Error('Stop the turn before changing permissions');
    const permissions = permissionSchema.parse(permission);
    saveThreadSettings(state, { ...state.settings, permissions });
    closeSession(state);
  }

  function setThreadPlanMode(threadId: string, plan: boolean) {
    const state = requireThread(threadId);
    if (isActive(state.turn)) throw new Error('Stop the turn before changing permissions');
    saveThreadSettings(state, { ...state.settings, plan });
    closeSession(state);
  }

  function removeQueuedMessage(threadId: string, messageId: string): boolean {
    const state = requireThread(threadId);
    const message = store.getClaudeMessage(messageId);
    if (!message || message.threadId !== threadId || !['queued', 'failed'].includes(message.status)) return false;
    if (isActive(state.turn) && state.turn.pending.has(message.id)) return false;
    updateMessage(message.id, 'cancelled');
    if (
      message.status === 'failed' &&
      !store.claudeMessages(threadId).some((item) => ['failed', 'unknown'].includes(item.status))
    )
      setQueueError(state, null);
    void dispatchQueue(threadId).catch(() => undefined);
    return true;
  }

  async function retryMessage(threadId: string, messageId: string): Promise<ClaudeMessage> {
    const state = requireThread(threadId);
    const message = store.getClaudeMessage(messageId);
    if (!message || message.threadId !== threadId || message.status !== 'failed')
      throw new Error('Only messages confirmed not to have executed can be retried');
    updateMessage(message.id, 'queued');
    setQueueError(state, null);
    await dispatchQueue(threadId);
    return store.getClaudeMessage(message.id) ?? message;
  }

  async function discardMessage(threadId: string, messageId: string): Promise<ClaudeMessage> {
    const state = requireThread(threadId);
    const message = store.getClaudeMessage(messageId);
    if (!message || message.threadId !== threadId || message.status !== 'unknown')
      throw new Error('Only messages with uncertain delivery can be discarded');
    const idle = state.turn;
    if (idle.kind !== 'idle') throw new Error('Stop the turn before discarding a message');
    const delivered = await deliveredMessageIds(threadId, state.settings.folder);
    if (requireThread(threadId) !== state || state.turn !== idle)
      throw new Error('Claude must be idle before discarding a message');
    if (delivered.has(message.id)) return updateMessage(message.id, 'accepted', message.turnId ?? message.id);
    // Discarding prevents a replay. It cannot undo work Claude may already have performed.
    return updateMessage(message.id, 'cancelled', message.turnId, message.error);
  }

  async function resumeQueue(threadId: string) {
    const state = requireThread(threadId);
    if (store.claudeMessages(threadId).some((message) => ['failed', 'unknown', 'submitting'].includes(message.status)))
      throw new Error('Resolve undelivered messages before resuming the queue');
    setQueueError(state, null);
    await dispatchQueue(threadId);
  }

  function registerThread(settings: ClaudeThread): ThreadState {
    store.saveClaudeThread(settings);
    const state: ThreadState = {
      settings,
      turn: { kind: 'idle' },
      stale: false,
      changes: createTurnChanges(),
      waiters: new Set(),
    };
    threads.set(settings.id, state);
    return state;
  }

  async function startThread(
    folder: string,
    settings: { model?: string; permissions?: Permission; plan?: boolean } = {},
  ): Promise<string> {
    if (closed) throw new Error('Claude provider is closed');
    const permissions = permissionSchema.parse(settings.permissions ?? 'askMe');
    const id = randomUUID();
    registerThread({
      id,
      folder,
      model: settings.model || null,
      permissions,
      plan: settings.plan ?? false,
      pausedError: null,
      runningTurn: null,
    });
    return Promise.resolve(id);
  }

  function reconcileMessages(threadId: string, delivered: Set<string>) {
    for (const message of store.claudeMessages(threadId)) {
      if (!['unknown', 'submitting'].includes(message.status)) continue;
      if (delivered.has(message.id)) updateMessage(message.id, 'accepted', message.turnId ?? message.id);
      else updateMessage(message.id, 'unknown', message.turnId, message.error ?? 'Delivery was interrupted');
    }
  }

  function restoreQueue(threadId: string, state: ThreadState) {
    if (state.settings.runningTurn)
      saveThreadSettings(state, {
        ...state.settings,
        pausedError: 'Last turn was interrupted before completion',
        runningTurn: null,
      });
    const unresolved = store.claudeMessages(threadId).find((message) => ['unknown', 'failed'].includes(message.status));
    if (unresolved && !state.settings.pausedError)
      setQueueError(state, unresolved.error ?? 'Resolve undelivered messages before resuming the queue');
  }

  async function resumeThread(threadId: string): Promise<string> {
    if (closed) throw new Error('Claude provider is closed');
    const current = threads.get(threadId);
    if (current && current.turn.kind !== 'unavailable') return threadId;
    const existing = resumes.get(threadId);
    if (existing) return existing;
    const resuming = (async () => {
      const saved = store.getClaudeThread(threadId);
      if (!saved) throw new Error('Unknown Claude thread ' + threadId);
      const delivered = await deliveredMessageIds(threadId, saved.folder);
      if (closed) throw new Error('Claude provider is closed');
      const state = registerThread(saved);
      try {
        reconcileMessages(threadId, delivered);
        restoreQueue(threadId, state);
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
    const { models } = await describeClaude(homedir());
    return new Map(
      models.map((model) => [model.value, { id: model.value, name: model.displayName, details: model.description }]),
    );
  }

  async function findSkills(folder: string) {
    const { commands } = await describeClaude(folder);
    return new Map(commands.map((command) => [command.name, { name: command.name, details: command.description }]));
  }

  async function findPlugins(folder: string) {
    const { plugins } = await describeClaude(folder);
    return new Map(plugins.map((plugin) => [plugin.path, plugin]));
  }

  async function isSignedIn() {
    const { account } = await describeClaude(homedir());
    const usable = (source: string | undefined) => source !== undefined && source !== 'none';
    // Cloud providers such as Bedrock authenticate outside Claude Code and report no token source.
    if ((account.apiProvider ?? 'firstParty') !== 'firstParty') return true;
    return account.email !== undefined || usable(account.tokenSource) || usable(account.apiKeySource);
  }

  function close(): Promise<void> {
    if (closed) return Promise.resolve();
    closed = true;
    const error = new Error('Claude provider closed');
    for (const [id, state] of threads) unavailable(id, state, error);
    clearRequests();
    for (const delivery of deliveries.values()) delivery.reject(error);
    deliveries.clear();
    return Promise.resolve();
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
    setThreadPlanMode,
    providerModels,
    findSkills,
    findPlugins,
    isSignedIn,
    onEvent,
    close,
    messages: (threadId: string) => store.claudeMessages(threadId),
    ...requests,
  };
}

export type ClaudeProvider = ReturnType<typeof createClaudeProvider>;
