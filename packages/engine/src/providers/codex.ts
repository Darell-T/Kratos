import { connectCodex, type v2 } from '@kratos/codex';

type UserInput = v2.UserInput;

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
type Permission = 'askMe' | 'approveForMe' | 'fullAccess';
type ApprovalDecision = 'accept' | 'acceptForSession' | 'decline' | 'cancel';

interface ThreadState {
  model?: string;
  turnId?: string;
  permissions?: Permission;
}
type TurnStartRequest = {
  threadId: string;
  input: UserInput[];
  model?: string;
  approvalsReviewer?: 'user' | 'auto_review';
  sandboxPolicy?: {
    type: 'dangerFullAccess';
  };
  approvalPolicy?: 'never';
};

const codex = await connectCodex({ clientInfo: { name: 'kratos', title: 'Kratos', version: '0.0.0' } });
const queuedMessages = new Map<string, UserInput[][]>();
const stoppedTurns = new Set<string>();
const threads = new Map<string, ThreadState>();
const pendingCommands = new Map<string, { command: string | null; resolve: (decision: ApprovalDecision) => void }>();
const pendingFiles = new Map<string, { reason: string | null; resolve: (decision: ApprovalDecision) => void }>();

function activity(item: v2.ThreadItem): string | undefined {
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

codex.onNotification('item/agentMessage/delta', (params) => process.stdout.write(params.delta));
codex.onNotification('item/started', (params) => {
  const line = activity(params.item);
  if (line) console.log(line);
});
codex.onServerRequest('item/commandExecution/requestApproval', (params) => {
  if (threads.get(params.threadId)?.permissions !== 'askMe') return { decision: 'accept' };

  console.log('approval needed', params.command);
  return new Promise<{ decision: ApprovalDecision }>((resolve) => {
    pendingCommands.get(params.threadId)?.resolve('cancel');
    pendingCommands.set(params.threadId, {
      command: params.command ?? null,
      resolve: (decision) => resolve({ decision }),
    });
  });
});
codex.onServerRequest('item/fileChange/requestApproval', (params) => {
  if (threads.get(params.threadId)?.permissions !== 'askMe') return { decision: 'accept' };
  return new Promise<{ decision: ApprovalDecision }>((resolve) => {
    pendingFiles.get(params.threadId)?.resolve('cancel');
    pendingFiles.set(params.threadId, {
      reason: params.reason ?? null,
      resolve: (decision) => resolve({ decision }),
    });
  });
});
codex.onNotification('turn/completed', (params) => {
  console.log();
  const state = threads.get(params.threadId);
  if (state) delete state.turnId;

  const waiting = queuedMessages.get(params.threadId);
  if (waiting?.length) {
    queuedMessages.delete(params.threadId);
    void startTurn(params.threadId, waiting.flat());
  }
});

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

if (!(await isSignedIn())) console.error('Codex is not signed in. Run: codex login');

const skills = await findSkills();
console.log(`${skills.size} skills:`, [...skills.keys()].join(', '));

const plugins = await findPlugins();
for (const [id, plugin] of plugins) console.log(`${id}: ${plugin.name} from ${plugin.marketplace}`);

async function startThread(folder: string) {
  const result = await codex.request('thread/start', { cwd: folder, ephemeral: true }); //dont save the damn test thread
  return result.thread.id;
}

export function setThreadModel(threadId: string, model: string) {
  const state = threads.get(threadId);
  if (state) state.model = model;
  else threads.set(threadId, { model });
}

export function commandWaiting(threadId: string): string | null | undefined {
  return pendingCommands.get(threadId)?.command;
}

export function decideCommand(threadId: string, decision: ApprovalDecision) {
  const pending = pendingCommands.get(threadId);
  if (!pending) return;
  pendingCommands.delete(threadId);
  pending.resolve(decision);
}
export function decideFile(threadId: string, decision: ApprovalDecision) {
  const pending = pendingFiles.get(threadId);
  if (!pending) return;
  pendingFiles.delete(threadId);
  pending.resolve(decision);
}

export function setThreadPermissions(threadId: string, permissions: Permission) {
  const state = threads.get(threadId);
  if (state) state.permissions = permissions;
  else threads.set(threadId, { permissions });
}

async function startTurn(threadId: string, input: UserInput[]) {
  // Busy from this moment, so a message sent before Codex confirms the turn still gets queued.
  const request: TurnStartRequest = {
    threadId,
    input,
  };
  const state = threads.get(threadId) ?? {};
  state.turnId = '';
  threads.set(threadId, state);
  if (state.model) {
    request.model = state.model;
  }
  if (state.permissions === 'askMe') {
    request.approvalsReviewer = 'user';
  }
  if (state.permissions === 'approveForMe') {
    request.approvalsReviewer = 'auto_review';
  }
  if (state.permissions === 'fullAccess') {
    request.sandboxPolicy = {
      type: 'dangerFullAccess',
    };
    request.approvalPolicy = 'never';
  }

  let turn: { id: string };
  try {
    ({ turn } = await codex.request('turn/start', request));
  } catch (error) {
    delete state.turnId;
    stoppedTurns.delete(threadId);
    throw error;
  }
  if (stoppedTurns.delete(threadId)) {
    delete state.turnId;
    await codex.request('turn/interrupt', { threadId, turnId: turn.id });
    return;
  }
  if (state.turnId !== undefined) {
    state.turnId = turn.id;
  }
}

export async function steerTurn(threadId: string, input: UserInput[]) {
  const turnId = threads.get(threadId)?.turnId;
  if (!turnId) return;

  const waiting = queuedMessages.get(threadId);
  if (waiting) {
    const steered = JSON.stringify(input);
    const index = waiting.findIndex((queued) => JSON.stringify(queued) === steered);
    if (index !== -1) waiting.splice(index, 1);
    if (waiting.length === 0) queuedMessages.delete(threadId);
  }

  await codex.request('turn/steer', { threadId, expectedTurnId: turnId, input });
}
export async function stopTurn(threadId: string) {
  const state = threads.get(threadId);
  const turnId = state?.turnId;
  if (!state || turnId === undefined) return;

  queuedMessages.delete(threadId);
  if (turnId === '') {
    stoppedTurns.add(threadId);
    return;
  }

  delete state.turnId;
  await codex.request('turn/interrupt', { threadId, turnId });
}

async function sendMessage(threadId: string, input: UserInput[]) {
  if (threads.get(threadId)?.turnId !== undefined) {
    const waiting = queuedMessages.get(threadId) ?? [];
    waiting.push(input);
    queuedMessages.set(threadId, waiting);
  } else {
    await startTurn(threadId, input);
  }
}

function waitUntilIdle(threadId: string) {
  return new Promise<void>((resolve) => {
    const stop = codex.onNotification('turn/completed', (params) => {
      if (params.threadId === threadId && threads.get(threadId)?.turnId === undefined) {
        stop();
        resolve();
      }
    });
  });
}

const threadId = await startThread(process.cwd());
console.log('opened thread', threadId);

await sendMessage(threadId, [{ type: 'text', text: 'What is 2 + 2? Answer with just the number.', text_elements: [] }]);
await sendMessage(threadId, [{ type: 'text', text: 'Now double it. Answer with just the number.', text_elements: [] }]);
await waitUntilIdle(threadId);

await codex.close();
