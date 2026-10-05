import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';

let initialized = false;

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
    send({ id: 'ask-1', method: params.method, params: params.params });
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

function handleClientAnswer({ result, error }) {
  send({ method: 'test/answered', params: { result, error } });
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
