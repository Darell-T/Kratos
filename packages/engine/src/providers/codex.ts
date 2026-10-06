import { connectCodex, type v2 } from '@kratos/codex';

type UserInput = v2.UserInput;

interface Plugin {
  name: string;
  marketplace: string;
  enabled: boolean;
}

const codex = await connectCodex({ clientInfo: { name: 'kratos', title: 'Kratos', version: '0.0.0' } });
const activeTurns = new Map<string, string>();
const queuedMessages = new Map<string, UserInput[][]>();

codex.onNotification('item/agentMessage/delta', (params) => process.stdout.write(params.delta));

codex.onNotification('turn/completed', (params) => {
  console.log();
  activeTurns.delete(params.threadId);

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

  const userSkills: Map<string, string> = new Map();
  for (const entry of codexSkills.data) {
    for (const skill of entry.skills) userSkills.set(skill.name, skill.description);
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

if (!(await isSignedIn())) console.error('Codex is not signed in. Run: codex login');

const skills = await findSkills();
console.log(`${skills.size} skills:`, [...skills.keys()].join(', '));

const plugins = await findPlugins();
for (const [id, plugin] of plugins) console.log(`${id}: ${plugin.name} from ${plugin.marketplace}`);

async function startThread(folder: string) {
  const result = await codex.request('thread/start', { cwd: folder, ephemeral: true }); //dont save the damn test thread
  return result.thread.id;
}

async function startTurn(threadId: string, input: UserInput[]) {
  // Busy from this moment, so a message sent before Codex confirms the turn still gets queued.
  activeTurns.set(threadId, '');
  const { turn } = await codex.request('turn/start', { threadId, input });
  if (activeTurns.has(threadId)) activeTurns.set(threadId, turn.id);
}

export async function steerTurn(threadId: string, input: UserInput[]) {
  const turnId = activeTurns.get(threadId);
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

async function sendMessage(threadId: string, input: UserInput[]) {
  if (activeTurns.has(threadId)) {
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
      if (params.threadId === threadId && !activeTurns.has(threadId)) {
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
