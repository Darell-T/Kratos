import { mkdtempSync, realpathSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { CodexExitedError, CodexNotFoundError, CodexRequestError, connectCodex } from '../src';
import type { CodexOptions } from '../src';

// The fake speaks made-up `test/*` methods that are not in the generated protocol, so tests use an untyped view.
interface FakeClient {
  info: { userAgent: string; codexHome: string };
  request(method: string, params?: unknown): Promise<unknown>;
  onNotification(method: string, handler: (params: unknown) => void): () => void;
  onServerRequest(method: string, handler: (params: unknown) => unknown): () => void;
  onExit(handler: (code: number | null) => void): () => void;
  close(): Promise<void>;
}

const fakeCodexPath = fileURLToPath(new URL('./fake-codex.mjs', import.meta.url));
const openClients: FakeClient[] = [];
const strayPids: number[] = [];

async function connectFake(options: CodexOptions = {}): Promise<FakeClient> {
  const client = (await connectCodex({
    command: process.execPath,
    args: [fakeCodexPath, 'app-server'],
    ...options,
  })) as unknown as FakeClient;
  openClients.push(client);
  return client;
}

// The fake answers `test/ask` right away and reports what the client replied to its own request afterwards.
async function askFake(client: FakeClient, method: string, params: unknown): Promise<unknown> {
  const answer = new Promise<unknown>((resolve) => {
    const stop = client.onNotification('test/answered', (value) => {
      stop();
      resolve(value);
    });
  });
  await client.request('test/ask', { method, params });
  return answer;
}

function isAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

afterEach(async () => {
  vi.restoreAllMocks();
  await Promise.all(openClients.splice(0).map((client) => client.close()));
  for (const pid of strayPids.splice(0)) {
    if (isAlive(pid)) process.kill(pid);
  }
});

describe('connectCodex', () => {
  it('completes the handshake and exposes the initialize result', async () => {
    const client = await connectFake({ clientInfo: { name: 'my-app', title: 'My App', version: '9.9.9' } });

    expect(client.info).toEqual({
      userAgent: 'my-app/9.9.9 (fake codex)',
      codexHome: 'C:\\fake\\.codex',
      platformFamily: 'windows',
      platformOs: 'windows',
    });
  });

  it('only accepts requests after the handshake has finished', async () => {
    const client = await connectFake();

    await expect(client.request('test/echo', { first: true })).resolves.toEqual({ first: true });
  });

  it('passes the working directory and CODEX_HOME to the process', async () => {
    const cwd = realpathSync(mkdtempSync(join(tmpdir(), 'kratos-codex-')));
    try {
      const client = await connectFake({ cwd, codexHome: 'C:\\somewhere\\.codex' });

      await expect(client.request('test/environment')).resolves.toEqual({ codexHome: 'C:\\somewhere\\.codex', cwd });
    } finally {
      await Promise.all(openClients.splice(0).map((client) => client.close()));
      rmSync(cwd, { recursive: true, force: true });
    }
  });

  it('rejects with CodexNotFoundError when the command does not exist', async () => {
    const connecting = connectCodex({ command: 'kratos-no-such-codex' });

    await expect(connecting).rejects.toBeInstanceOf(CodexNotFoundError);
    await expect(connecting).rejects.toThrow(/npm install -g @openai\/codex.*CODEX_EXE/);
  });
});

describe('request', () => {
  it('returns the result Codex sends back', async () => {
    const client = await connectFake();

    await expect(client.request('test/echo', { hello: 'world', n: [1, 2] })).resolves.toEqual({
      hello: 'world',
      n: [1, 2],
    });
  });

  it('rejects with CodexRequestError carrying the code and message', async () => {
    const client = await connectFake();

    const failure = client.request('test/fail').catch((error: unknown) => error);

    expect(await failure).toBeInstanceOf(CodexRequestError);
    expect(await failure).toMatchObject({ code: -32000, message: 'boom' });
  });

  it('ignores output lines that are not JSON', async () => {
    const client = await connectFake();

    await expect(client.request('test/noise')).resolves.toBe('still alive');
  });
});

describe('onNotification', () => {
  it('delivers a notification to every handler of its method and to no other', async () => {
    const client = await connectFake();
    const first: unknown[] = [];
    const second: unknown[] = [];
    const other: unknown[] = [];
    client.onNotification('test/notified', (params) => first.push(params));
    client.onNotification('test/notified', (params) => second.push(params));
    client.onNotification('test/other', (params) => other.push(params));

    // The fake sends the notification before the reply, so it has been delivered once the request settles.
    await client.request('test/notify', { n: 1 });

    expect(first).toEqual([{ n: 1 }]);
    expect(second).toEqual([{ n: 1 }]);
    expect(other).toEqual([]);
  });

  it('stops delivering after unsubscribe without affecting other handlers', async () => {
    const client = await connectFake();
    const first: unknown[] = [];
    const second: unknown[] = [];
    const unsubscribeFirst = client.onNotification('test/notified', (params) => first.push(params));
    client.onNotification('test/notified', (params) => second.push(params));
    await client.request('test/notify', { n: 1 });

    unsubscribeFirst();
    await client.request('test/notify', { n: 2 });

    expect(first).toEqual([{ n: 1 }]);
    expect(second).toEqual([{ n: 1 }, { n: 2 }]);
  });

  it('keeps delivering when one handler throws', async () => {
    const logged = vi.spyOn(console, 'error').mockImplementation(() => undefined);
    const client = await connectFake();
    const received: unknown[] = [];
    client.onNotification('test/notified', () => {
      throw new Error('bad subscriber');
    });
    client.onNotification('test/notified', (params) => received.push(params));

    await client.request('test/notify', { n: 1 });

    expect(received).toEqual([{ n: 1 }]);
    expect(logged).toHaveBeenCalledOnce();
  });
});

describe('onServerRequest', () => {
  it('answers a server request with the handler result under the same id', async () => {
    const client = await connectFake();
    const seen: unknown[] = [];
    client.onServerRequest('test/question', (params) => {
      seen.push(params);
      return Promise.resolve({ decision: 'accept' });
    });

    const answer = await askFake(client, 'test/question', { command: 'ls' });

    expect(seen).toEqual([{ command: 'ls' }]);
    expect(answer).toEqual({ result: { decision: 'accept' } });
  });

  it('answers -32601 right away when no handler is registered', async () => {
    const client = await connectFake();

    const answer = await askFake(client, 'test/unhandled', {});

    expect(answer).toEqual({ error: { code: -32601, message: 'Method not handled by Kratos: test/unhandled' } });
  });

  it('answers -32601 again after the handler is unsubscribed', async () => {
    const client = await connectFake();
    const unsubscribe = client.onServerRequest('test/question', () => 'yes');
    unsubscribe();

    const answer = await askFake(client, 'test/question', {});

    expect(answer).toMatchObject({ error: { code: -32601 } });
  });

  it('lets a second registration replace the first', async () => {
    const client = await connectFake();
    client.onServerRequest('test/question', () => 'first');
    client.onServerRequest('test/question', () => 'second');

    const answer = await askFake(client, 'test/question', {});

    expect(answer).toEqual({ result: 'second' });
  });

  it('answers -32603 with the message when the handler throws', async () => {
    const client = await connectFake();
    client.onServerRequest('test/question', () => {
      throw new Error('handler exploded');
    });

    const answer = await askFake(client, 'test/question', {});

    expect(answer).toEqual({ error: { code: -32603, message: 'handler exploded' } });
  });
});

describe('when the process exits', () => {
  it('rejects pending requests, fires onExit once and rejects later requests', async () => {
    const client = await connectFake();
    const exitCodes: (number | null)[] = [];
    client.onExit((code) => exitCodes.push(code));

    const hanging = client.request('test/hang').catch((error: unknown) => error);
    const exiting = client.request('test/exit', { code: 3 }).catch((error: unknown) => error);

    for (const outcome of [await hanging, await exiting]) {
      expect(outcome).toBeInstanceOf(CodexExitedError);
      expect(outcome).toMatchObject({ exitCode: 3 });
    }
    expect(exitCodes).toEqual([3]);
    await expect(client.request('test/echo', {})).rejects.toBeInstanceOf(CodexExitedError);
    expect(exitCodes).toEqual([3]);
  });

  it('calls an onExit handler registered after the exit straight away', async () => {
    const client = await connectFake();
    await client.request('test/exit', { code: 4 }).catch(() => undefined);

    const exitCodes: (number | null)[] = [];
    client.onExit((code) => exitCodes.push(code));

    expect(exitCodes).toEqual([4]);
  });

  it('does not call an onExit handler that was unsubscribed', async () => {
    const client = await connectFake();
    const exitCodes: (number | null)[] = [];
    client.onExit((code) => exitCodes.push(code))();

    await client.request('test/exit', { code: 1 }).catch(() => undefined);

    expect(exitCodes).toEqual([]);
  });
});

describe('close', () => {
  it('ends the process and tells onExit handlers', async () => {
    const client = await connectFake();
    const pid = (await client.request('test/pid')) as number;
    const exitCodes: (number | null)[] = [];
    client.onExit((code) => exitCodes.push(code));
    expect(isAlive(pid)).toBe(true);

    await client.close();

    expect(isAlive(pid)).toBe(false);
    expect(exitCodes).toHaveLength(1);
    await expect(client.request('test/echo', {})).rejects.toBeInstanceOf(CodexExitedError);
  });

  it('can be called again after the process is gone', async () => {
    const client = await connectFake();
    await client.close();

    await expect(client.close()).resolves.toBeUndefined();
  });

  // Elsewhere SIGTERM only reaches the direct child, which is all the spec asks for there.
  it.runIf(process.platform === 'win32')('stops processes the child started, not just the child', async () => {
    const client = await connectFake();
    const childPid = (await client.request('test/spawnChild')) as number;
    strayPids.push(childPid);
    expect(isAlive(childPid)).toBe(true);

    await client.close();

    await expect.poll(() => isAlive(childPid)).toBe(false);
  });
});
