import { connectCodex, type v2 } from '@kratos/codex';
import { createCodexApprovals, type Permission } from './codex-approvals';

type UserInput = v2.UserInput;

export type CodexEvent =
  | ({ kind: 'message.delta' } & v2.AgentMessageDeltaNotification)
  | ({ kind: 'item.started' } & v2.ItemStartedNotification)
  | ({ kind: 'item.completed' } & v2.ItemCompletedNotification)
  | ({ kind: 'turn.started' } & v2.TurnStartedNotification)
  | ({ kind: 'turn.completed' } & v2.TurnCompletedNotification)
  | ({ kind: 'error' } & v2.ErrorNotification)
  | { kind: 'turn.startFailed'; threadId: string; message: string }
  | { kind: 'provider.exited'; exitCode: number | null };

interface Plugin {
  name: string;
  marketplace: string;
  enabled: boolean;
}
interface Skill {
  details: string;
  path: string;
}
interface Model {
  id: string;
  details: string;
}

interface StartingTurn {
  kind: 'starting';
  startPromise: Promise<ActiveTurn | undefined>;
  completedTurnIds: Set<string>;
  stopPromise?: Promise<void>;
}

interface ActiveTurn {
  kind: 'running' | 'stopping';
  turnId: string;
  stopPromise?: Promise<void>;
}

type TurnState = { kind: 'idle' } | StartingTurn | ActiveTurn;

interface ThreadState {
  model?: string;
  turn: TurnState;
  permissions?: Permission;
}

type TurnStartRequest = {
  threadId: string;
  input: UserInput[];
  model?: string;
  approvalsReviewer?: 'user' | 'auto_review';
  sandboxPolicy?: v2.SandboxPolicy;
  approvalPolicy?: v2.AskForApproval;
};

const codex = await connectCodex({ clientInfo: { name: 'kratos', title: 'Kratos', version: '0.0.0' } });
const queuedMessages = new Map<string, UserInput[][]>();
const threads = new Map<string, ThreadState>();
const eventListeners = new Set<(event: CodexEvent) => void>();
export const { commandWaiting, fileWaiting, approvalsWaiting, decideCommand, decideFile } = createCodexApprovals(
  codex,
  (threadId) => threads.get(threadId)?.permissions,
);

export function onEvent(listener: (event: CodexEvent) => void): () => void {
  eventListeners.add(listener);
  return () => {
    eventListeners.delete(listener);
  };
}

function emitEvent(event: CodexEvent) {
  for (const listener of eventListeners) {
    try {
      listener(event);
    } catch (error) {
      console.error('Codex event listener threw', error);
    }
  }
}

function describeActivity(item: v2.ThreadItem): string | undefined {
  if (item.type === 'fileChange') {
    const paths = item.changes.map((change) => change.path).join(', ');
    return paths ? `editing ${paths}` : 'editing';
  }
  if (item.type === 'imageView') return `viewing ${item.path}`;
  if (item.type !== 'commandExecution') return;

  const reads = item.commandActions.filter((action) => action.type === 'read');
  if (reads.length > 0 && reads.length === item.commandActions.length) {
    return `reading ${reads.map((action) => action.path).join(', ')}`;
  }
  return `running ${item.command}`;
}

codex.onNotification('item/agentMessage/delta', (params) => emitEvent({ kind: 'message.delta', ...params }));
codex.onNotification('item/started', (params) => emitEvent({ kind: 'item.started', ...params }));
codex.onNotification('item/completed', (params) => emitEvent({ kind: 'item.completed', ...params }));
codex.onNotification('turn/started', (params) => emitEvent({ kind: 'turn.started', ...params }));
codex.onNotification('turn/completed', (params) => {
  completeTurn(params.threadId, params.turn.id);
  emitEvent({ kind: 'turn.completed', ...params });
});
codex.onNotification('error', (params) => emitEvent({ kind: 'error', ...params }));
codex.onExit((exitCode) => emitEvent({ kind: 'provider.exited', exitCode }));

async function isSignedIn() {
  const { account } = await codex.request('account/read', {});
  return account !== null;
}

async function findSkills() {
  const codexSkills = await codex.request('skills/list', { cwds: [process.cwd()] });

  const userSkills: Map<string, Skill> = new Map();
  for (const entry of codexSkills.data) {
    for (const skill of entry.skills) {
      const skillData: Skill = { details: skill.description, path: skill.path };
      userSkills.set(skill.name, skillData);
    }
  }
  return userSkills;
}

async function findPlugins() {
  const codexPlugins = await codex.request('plugin/installed', { cwds: [process.cwd()] });

  const userPlugins: Map<string, Plugin> = new Map();
  for (const marketplace of codexPlugins.marketplaces) {
    for (const plugin of marketplace.plugins) {
      if (plugin.installed) {
        userPlugins.set(plugin.id, { name: plugin.name, marketplace: marketplace.name, enabled: plugin.enabled });
      }
    }
  }
  return userPlugins;
}

export async function providerModels() {
  const codexModels: Map<string, Model> = new Map();
  const models = await codex.request('model/list', { cursor: null, limit: null, includeHidden: null });

  for (const entry of models.data) {
    const model: Model = { id: entry.model, details: entry.description };
    codexModels.set(entry.displayName, model);
  }
  return codexModels;
}

async function startThread(folder: string) {
  const result = await codex.request('thread/start', { cwd: folder, ephemeral: true });
  setThreadPermissions(result.thread.id, 'askMe');
  return result.thread.id;
}

export function setThreadModel(threadId: string, model: string) {
  const state = threads.get(threadId);
  if (state) state.model = model;
  else threads.set(threadId, { model, turn: { kind: 'idle' } });
}

export function setThreadPermissions(threadId: string, permissions: Permission) {
  const state = threads.get(threadId);
  if (state) state.permissions = permissions;
  else threads.set(threadId, { permissions, turn: { kind: 'idle' } });
}

export async function sendMessage(threadId: string, input: UserInput[]) {
  const state = threads.get(threadId);
  if (state && state.turn.kind !== 'idle') {
    const waiting = queuedMessages.get(threadId) ?? [];
    waiting.push(input);
    queuedMessages.set(threadId, waiting);
  } else {
    await startTurn(threadId, input);
  }
}

async function startTurn(threadId: string, input: UserInput[]) {
  const request: TurnStartRequest = {
    threadId,
    input,
  };
  const state: ThreadState = threads.get(threadId) ?? { turn: { kind: 'idle' } };
  threads.set(threadId, state);
  if (state.model) {
    request.model = state.model;
  }
  if (state.permissions === 'askMe') {
    request.sandboxPolicy = {
      type: 'workspaceWrite',
      writableRoots: [],
      networkAccess: false,
      excludeTmpdirEnvVar: false,
      excludeSlashTmp: false,
    };
    request.approvalPolicy = 'on-request';
    request.approvalsReviewer = 'user';
  }
  if (state.permissions === 'approveForMe') {
    request.sandboxPolicy = {
      type: 'workspaceWrite',
      writableRoots: [],
      networkAccess: false,
      excludeTmpdirEnvVar: false,
      excludeSlashTmp: false,
    };

    request.approvalsReviewer = 'auto_review';
    request.approvalPolicy = 'on-request';
  }
  if (state.permissions === 'fullAccess') {
    request.sandboxPolicy = {
      type: 'dangerFullAccess',
    };
    request.approvalPolicy = 'never';
  }

  const startingTurn: StartingTurn = {
    kind: 'starting',
    completedTurnIds: new Set(),
    startPromise: codex.request('turn/start', request).then(
      ({ turn }) => {
        if (state.turn !== startingTurn) return;
        const activeTurn: ActiveTurn = { kind: 'running', turnId: turn.id };
        if (startingTurn.stopPromise) activeTurn.stopPromise = startingTurn.stopPromise;
        state.turn = activeTurn;
        if (startingTurn.completedTurnIds.has(turn.id) || turn.status !== 'inProgress') {
          completeTurn(threadId, turn.id);
          return;
        }
        return activeTurn;
      },
      (error: unknown) => {
        if (state.turn === startingTurn) state.turn = { kind: 'idle' };
        emitEvent({
          kind: 'turn.startFailed',
          threadId,
          message: error instanceof Error ? error.message : String(error),
        });
        throw error;
      },
    ),
  };
  state.turn = startingTurn;
  await startingTurn.startPromise;
}

function completeTurn(threadId: string, turnId: string) {
  const state = threads.get(threadId);
  if (!state || state.turn.kind === 'idle') return;
  if (state.turn.kind === 'starting') {
    state.turn.completedTurnIds.add(turnId);
    return;
  }
  if (state.turn.turnId !== turnId) return;
  state.turn = { kind: 'idle' };

  const waiting = queuedMessages.get(threadId);
  if (waiting?.length) {
    queuedMessages.delete(threadId);
    void startTurn(threadId, waiting.flat()).catch(() => undefined);
  }
}

export async function steerTurn(threadId: string, input: UserInput[]) {
  const turn = threads.get(threadId)?.turn;
  if (turn?.kind !== 'running') return;

  const waiting = queuedMessages.get(threadId);
  if (waiting) {
    const steered = JSON.stringify(input);
    const index = waiting.findIndex((queued) => JSON.stringify(queued) === steered);
    if (index !== -1) waiting.splice(index, 1);
    if (waiting.length === 0) queuedMessages.delete(threadId);
  }

  await codex.request('turn/steer', { threadId, expectedTurnId: turn.turnId, input });
}

export async function stopTurn(threadId: string) {
  const state = threads.get(threadId);
  const turn = state?.turn;
  if (!state || !turn || turn.kind === 'idle') return;
  if (turn.stopPromise) return turn.stopPromise;

  const interrupt = async () => {
    const activeTurn = turn.kind === 'starting' ? await turn.startPromise : turn;
    if (!activeTurn || state.turn !== activeTurn) return;
    activeTurn.kind = 'stopping';
    try {
      await codex.request('turn/interrupt', { threadId, turnId: activeTurn.turnId });
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

function waitUntilIdle(threadId: string) {
  return new Promise<void>((resolve) => {
    const unsubscribe = codex.onNotification('turn/completed', (params) => {
      const state = threads.get(threadId);
      if (params.threadId === threadId && (!state || state.turn.kind === 'idle')) {
        unsubscribe();
        resolve();
      }
    });
  });
}

if (import.meta.main) {
  const unsubscribe = onEvent((event) => {
    if (event.kind === 'message.delta') process.stdout.write(event.delta);
    if (event.kind === 'item.started') {
      const line = describeActivity(event.item);
      if (line) console.log(line);
    }
    if (event.kind === 'turn.completed') console.log();
    if (event.kind === 'error') console.error(event.error.message);
    if (event.kind === 'turn.startFailed') console.error('Codex turn failed to start', event.message);
  });

  if (!(await isSignedIn())) console.error('Codex is not signed in. Run: codex login');

  const skills = await findSkills();
  console.log(`${skills.size} skills:`, [...skills.keys()].join(', '));

  const plugins = await findPlugins();
  for (const [id, plugin] of plugins) console.log(`${id}: ${plugin.name} from ${plugin.marketplace}`);

  const threadId = await startThread(process.cwd());
  console.log('opened thread', threadId);

  await sendMessage(threadId, [
    { type: 'text', text: 'What is 2 + 2? Answer with just the number.', text_elements: [] },
  ]);
  await sendMessage(threadId, [
    { type: 'text', text: 'Now double it. Answer with just the number.', text_elements: [] },
  ]);
  await waitUntilIdle(threadId);

  await codex.close();
  unsubscribe();
}
