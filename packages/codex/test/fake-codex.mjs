import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';

let initialized = false;
const turnRequests = [];
const pendingTurns = new Map();

function holdTurn(request) {
  turnRequests.push({ method: request.method, params: request.params });
  pendingTurns.set(request.id, request);
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
  'turn/interrupt': holdTurn,
  'test/turnRequests': ({ id }) => reply(id, turnRequests),
  'test/settleTurn': ({ id, params }) => {
    const pending = [...pendingTurns.values()].find(
      (request) => request.method === params.method && request.params.threadId === params.threadId,
    );
    if (!pending) return fail(id, -32000, 'No matching pending turn request');
    pendingTurns.delete(pending.id);
    if (params.error) fail(pending.id, -32000, params.error);
    else reply(pending.id, params.result);
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
