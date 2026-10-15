import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';

let initialized = false;
const turnRequests = [];
const pendingTurns = new Map();
const threads = new Map();
let threadNumber = 0;
let modelPages = [{ data: [], nextCursor: null }];

function threadData(id, cwd = process.cwd()) {
  return { id, cwd, model: 'fake-model', status: { type: 'idle' }, turns: [], ephemeral: false };
}

function threadReply(thread) {
  return { thread, model: thread.model, cwd: thread.cwd };
}

function holdTurn(request) {
  turnRequests.push({ method: request.method, params: request.params });
  pendingTurns.set(request.id, request);
}

function rememberInput({ method, params }, result) {
  const thread = threads.get(params.threadId);
  if (!thread) return;
  const message = {
    type: 'userMessage',
    id: params.clientUserMessageId,
    clientId: params.clientUserMessageId,
    content: params.input,
  };
  if (method === 'turn/start' && result?.turn) {
    const turn = { ...result.turn, items: [...result.turn.items, message] };
    thread.turns.push(turn);
    thread.status = { type: turn.status === 'inProgress' ? 'active' : 'idle', activeFlags: [] };
  } else if (method === 'turn/steer') {
    const turn = thread.turns.find((turn) => turn.id === params.expectedTurnId);
    turn?.items.push(message);
  }
}

function send(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function reply(id, result) {
  send({ id, result });
}

function fail(id, code, message) {
  send({ id, error: { code, message } });
}

const handlers = {
  'turn/start': holdTurn,
  'turn/steer': holdTurn,
  'turn/interrupt': holdTurn,
  'thread/start': ({ id, params }) => {
    const thread = threadData('thread-' + String.fromCharCode(65 + threadNumber++), params.cwd);
    if (params.model) thread.model = params.model;
    threads.set(thread.id, thread);
    reply(id, threadReply(thread));
  },
  'thread/resume': ({ id, params }) => {
    const thread = threads.get(params.threadId) ?? threadData(params.threadId, params.cwd);
    if (params.model) thread.model = params.model;
    threads.set(thread.id, thread);
    reply(id, threadReply(thread));
  },
  'thread/read': ({ id, params }) => reply(id, { thread: threads.get(params.threadId) ?? threadData(params.threadId) }),
  'model/list': ({ id, params }) => reply(id, modelPages[params.cursor ? Number(params.cursor) : 0]),
  'skills/list': ({ id, params }) =>
    reply(id, {
      data: [
        {
          cwd: params.cwds[0],
          skills: [{ name: 'project', description: 'Project skill', path: params.cwds[0] + '/SKILL.md' }],
        },
      ],
    }),
  'plugin/installed': ({ id, params }) =>
    reply(id, {
      marketplaces: [
        { name: params.cwds[0], plugins: [{ id: 'project', name: 'Project plugin', installed: true, enabled: true }] },
      ],
    }),
  'account/read': ({ id }) => reply(id, { account: { type: 'chatgpt' } }),
  'test/setThread': ({ id, params }) => {
    threads.set(params.thread.id, params.thread);
    reply(id, null);
  },
  'test/models': ({ id, params }) => {
    modelPages = params.pages;
    reply(id, null);
  },
  'test/turnRequests': ({ id }) => reply(id, turnRequests),
  'test/settleTurn': ({ id, params }) => {
    const pending = [...pendingTurns.values()].find(
      (request) => request.method === params.method && request.params.threadId === params.threadId,
    );
    if (!pending) return fail(id, -32000, 'No matching pending turn request');
    pendingTurns.delete(pending.id);
    if (params.error) fail(pending.id, params.code ?? -32000, params.error);
    else {
      rememberInput(pending, params.result);
      reply(pending.id, params.result);
    }
    reply(id, null);
  },
  'test/echo': ({ id, params }) => reply(id, params),
  'test/fail': ({ id }) => fail(id, -32000, 'boom'),
  'test/environment': ({ id }) => reply(id, { codexHome: process.env.CODEX_HOME, cwd: process.cwd() }),
  'test/notify': ({ id, params }) => {
    send({ method: 'test/notified', params });
    reply(id, null);
  },
  'test/noise': ({ id }) => {
    process.stdout.write('this is not json\n');
    reply(id, 'still alive');
  },
  'test/pid': ({ id }) => reply(id, process.pid),
  'test/ask': ({ id, params }) => {
    send({ id: params.requestId ?? 'ask-1', method: params.method, params: params.params });
    reply(id, null);
  },
  'test/emit': ({ id, params }) => {
    if (params.method === 'turn/completed') {
      const thread = threads.get(params.params.threadId);
      const turn = thread?.turns.find((turn) => turn.id === params.params.turn.id);
      if (turn) {
        const items = turn.items;
        Object.assign(turn, params.params.turn);
        if (!turn.items.length) turn.items = items;
      }
      if (thread) thread.status = { type: 'idle' };
    }
    send({ method: params.method, params: params.params });
    reply(id, null);
  },
  'test/spawnChild': ({ id }) => {
    // Detached, because node otherwise ties a child's lifetime to its parent and hides what the client must kill.
    const child = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: 'ignore', detached: true });
    child.unref();
    reply(id, child.pid);
  },
  'test/exit': ({ params }) => process.exit(params.code),
  // Never answers, so the client has a request in flight when the process goes away.
  'test/hang': () => undefined,
};

function handleRequest(request) {
  const { id, method, params } = request;
  if (method === 'initialize') {
    if (process.argv.includes('--hang-initialize')) return;
    if (process.argv.includes('--bad-initialize')) return reply(id, {});
    reply(id, {
      userAgent: `${params.clientInfo.name}/${params.clientInfo.version} (fake codex)`,
      codexHome: 'C:\\fake\\.codex',
      platformFamily: 'windows',
      platformOs: 'windows',
    });
  } else if (!initialized) {
    fail(id, -32002, 'Not initialized');
  } else if (handlers[method]) {
    handlers[method](request);
  } else {
    fail(id, -32601, `Method not found: ${method}`);
  }
}

function handleClientAnswer({ id, result, error }) {
  send({ method: 'test/answered', params: { result, error } });
  send({ method: 'test/answeredWithId', params: { id, result, error } });
}

createInterface({ input: process.stdin }).on('line', (line) => {
  const message = JSON.parse(line);
  if (message.method === 'initialized') {
    initialized = true;
  } else if (message.method !== undefined) {
    handleRequest(message);
  } else {
    handleClientAnswer(message);
  }
});
