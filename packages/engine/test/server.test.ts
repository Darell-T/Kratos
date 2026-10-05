import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { request } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { WebSocket } from 'ws';
import { Hub } from '../src/hub';
import { startServer, type RunningServer } from '../src/server';
import { Store } from '../src/store';

let dir: string;
let store: Store;
let server: RunningServer;
const sockets: WebSocket[] = [];

async function boot(uiDist = join(dir, 'no-ui')) {
  store = new Store(join(dir, 'home'));
  server = await startServer({ store, hub: new Hub(), uiDist, port: 0 });
}

const http = (path: string) => `http://127.0.0.1:${server.port}${path}`;

async function send(method: string, path: string, body?: unknown) {
  const response = await fetch(http(path), {
    method,
    ...(body === undefined ? {} : { headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) }),
  });
  const text = await response.text();
  return { status: response.status, text, json: (): unknown => JSON.parse(text) };
}

async function createThread() {
  const workspace = (await send('POST', '/api/workspaces', { name: 'repo', path: dir })).json() as { id: string };
  return (
    await send('POST', '/api/threads', { workspaceId: workspace.id, title: 'chat', provider: 'claude' })
  ).json() as {
    id: string;
  };
}

async function connect() {
  const socket = new WebSocket(`ws://127.0.0.1:${server.port}/api/events`);
  sockets.push(socket);
  const received: unknown[] = [];
  socket.on('message', (data) => received.push(JSON.parse((data as Buffer).toString('utf8'))));
  await new Promise((resolve, reject) => {
    socket.once('open', resolve);
    socket.once('error', reject);
  });
  return { socket, received };
}

const eventSeqs = (received: unknown[]) => received.map((message) => (message as { event: { seq: number } }).event.seq);

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), 'kratos-server-'));
});

afterEach(async () => {
  for (const socket of sockets.splice(0)) socket.terminate();
  await server.close();
  store.close();
  rmSync(dir, { recursive: true, force: true });
});

describe('HTTP API', () => {
  beforeEach(() => boot());

  it('answers the health check', async () => {
    const response = await send('GET', '/api/health');
    expect(response.status).toBe(200);
    expect(response.json()).toEqual({ ok: true });
  });

  it('creates a workspace, a thread and a message', async () => {
    const workspaceResponse = await send('POST', '/api/workspaces', { name: ' repo ', path: dir });
    expect(workspaceResponse.status).toBe(201);
    const workspace = workspaceResponse.json() as { id: string; name: string; path: string };
    expect(workspace).toEqual(expect.objectContaining({ name: 'repo', path: dir }));
    expect((await send('GET', '/api/workspaces')).json()).toEqual([workspace]);

    const threadResponse = await send('POST', '/api/threads', {
      workspaceId: workspace.id,
      title: 'first chat',
      provider: 'codex',
    });
    expect(threadResponse.status).toBe(201);
    const thread = threadResponse.json() as { id: string };
    expect((await send('GET', `/api/threads?workspaceId=${workspace.id}`)).json()).toEqual([thread]);

    const first = await send('POST', `/api/threads/${thread.id}/messages`, { text: '  hello  ' });
    expect(first.status).toBe(201);
    expect(first.json()).toEqual(
      expect.objectContaining({ threadId: thread.id, seq: 1, event: { kind: 'user.message', text: 'hello' } }),
    );
    const second = await send('POST', `/api/threads/${thread.id}/messages`, { text: 'again' });
    expect(second.json()).toEqual(expect.objectContaining({ seq: 2 }));
  });

  it.each([
    ['a workspace without a name', '/api/workspaces', { name: '  ', path: tmpdir() }],
    ['a workspace path that does not exist', '/api/workspaces', { name: 'x', path: '/definitely/not/here' }],
    ['a relative workspace path', '/api/workspaces', { name: 'x', path: '.' }],
    ['a thread with an unknown provider', '/api/threads', { workspaceId: 'w', title: 't', provider: 'gemini' }],
    ['a thread without a title', '/api/threads', { workspaceId: 'w', provider: 'codex' }],
  ])('rejects %s with 400', async (_name, path, body) => {
    const response = await send('POST', path, body);
    expect(response.status).toBe(400);
    expect((response.json() as { error: string }).error).not.toBe('');
  });

  it('rejects a workspace path that is a file', async () => {
    const file = join(dir, 'file.txt');
    writeFileSync(file, 'x');
    expect((await send('POST', '/api/workspaces', { name: 'x', path: file })).status).toBe(400);
  });

  it('rejects malformed and blank message bodies with 400', async () => {
    const thread = await createThread();
    const path = `/api/threads/${thread.id}/messages`;
    expect((await send('POST', path, { text: '   ' })).status).toBe(400);
    expect((await send('POST', path, {})).status).toBe(400);
    const notJson = await fetch(http(path), { method: 'POST', body: '{nope' });
    expect(notJson.status).toBe(400);
    expect(store.eventsAfter(thread.id, 0)).toEqual([]);
  });

  it('answers 404 for unknown threads, workspaces and API paths', async () => {
    expect((await send('POST', '/api/threads/nope/messages', { text: 'hi' })).status).toBe(404);
    expect((await send('POST', '/api/threads', { workspaceId: 'nope', title: 't', provider: 'codex' })).status).toBe(
      404,
    );
    expect((await send('GET', '/api/threads?workspaceId=nope')).status).toBe(404);
    expect((await send('GET', '/api/threads')).status).toBe(400);
    expect((await send('GET', '/api/nothing')).status).toBe(404);
  });
});

describe('UI files', () => {
  it('explains how to build the UI when the dist folder is missing', async () => {
    await boot();
    const response = await send('GET', '/');
    expect(response.status).toBe(503);
    expect(response.text).toContain('npm run build');
  });

  it('serves assets, falls back to index.html for app routes and 404s for missing assets', async () => {
    const uiDist = join(dir, 'ui');
    mkdirSync(join(uiDist, 'assets'), { recursive: true });
    writeFileSync(join(uiDist, 'index.html'), '<main>app</main>');
    writeFileSync(join(uiDist, 'assets', 'app.js'), 'console.log(1)');
    await boot(uiDist);

    expect((await send('GET', '/')).text).toBe('<main>app</main>');
    expect((await send('GET', '/threads/abc')).text).toBe('<main>app</main>');
    const asset = await fetch(http('/assets/app.js'));
    expect(asset.headers.get('content-type')).toContain('text/javascript');
    expect(await asset.text()).toBe('console.log(1)');
    expect((await send('GET', '/assets/missing.js')).status).toBe(404);
    expect((await send('GET', '/..%2f..%2fetc/passwd')).status).toBe(404);
  });
});

describe('request origin', () => {
  beforeEach(() => boot());

  function rawRequest(method: string, path: string, headers: Record<string, string>) {
    return new Promise<number>((resolve, reject) => {
      const req = request({ host: '127.0.0.1', port: server.port, method, path, headers }, (res) => {
        res.resume();
        resolve(res.statusCode ?? 0);
      });
      req.on('error', reject);
      req.end(method === 'POST' ? JSON.stringify({ name: 'x', path: dir }) : undefined);
    });
  }

  function socketOutcome(origin: string) {
    const socket = new WebSocket(`ws://127.0.0.1:${server.port}/api/events`, { origin });
    sockets.push(socket);
    return new Promise<string>((resolve) => {
      socket.once('open', () => resolve('open'));
      socket.once('error', () => resolve('rejected'));
    });
  }

  it('accepts local pages and clients that send no origin', async () => {
    expect(await rawRequest('GET', '/api/health', { host: `localhost:${server.port}` })).toBe(200);
    expect(
      await rawRequest('POST', '/api/workspaces', {
        host: `127.0.0.1:${server.port}`,
        origin: 'http://localhost:5173',
      }),
    ).toBe(201);
    expect(await socketOutcome(`http://localhost:${server.port}`)).toBe('open');
  });

  it('refuses other websites and other host names', async () => {
    const host = `127.0.0.1:${server.port}`;
    expect(await rawRequest('POST', '/api/workspaces', { host, origin: 'https://example.com' })).toBe(403);
    expect(await rawRequest('GET', '/api/workspaces', { host, origin: 'https://example.com' })).toBe(403);
    expect(await rawRequest('GET', '/api/health', { host: `attacker.example:${server.port}` })).toBe(403);
    expect(await socketOutcome('https://example.com')).toBe('rejected');
    expect(store.listWorkspaces()).toEqual([]);
  });
});

describe('WebSocket events', () => {
  beforeEach(() => boot());

  it('replays the backlog and then streams live events, each exactly once', async () => {
    const thread = await createThread();
    await send('POST', `/api/threads/${thread.id}/messages`, { text: 'one' });
    await send('POST', `/api/threads/${thread.id}/messages`, { text: 'two' });

    const { socket, received } = await connect();
    socket.send(JSON.stringify({ type: 'subscribe', threadId: thread.id, after: 0 }));
    await vi.waitFor(() => expect(eventSeqs(received)).toEqual([1, 2]));

    await send('POST', `/api/threads/${thread.id}/messages`, { text: 'three' });
    await vi.waitFor(() => expect(eventSeqs(received)).toEqual([1, 2, 3]));

    await new Promise((resolve) => setTimeout(resolve, 100));
    expect(eventSeqs(received)).toEqual([1, 2, 3]);
  });

  it('loses and repeats nothing when messages arrive while subscribing', async () => {
    const thread = await createThread();
    const post = (text: string) => send('POST', `/api/threads/${thread.id}/messages`, { text });
    await Promise.all([post('a'), post('b'), post('c')]);

    const { socket, received } = await connect();
    socket.send(JSON.stringify({ type: 'subscribe', threadId: thread.id, after: 0 }));
    await Promise.all([post('d'), post('e'), post('f')]);

    await vi.waitFor(() => expect(received).toHaveLength(6));
    await new Promise((resolve) => setTimeout(resolve, 100));
    expect(eventSeqs(received)).toEqual([1, 2, 3, 4, 5, 6]);
  });

  it('resumes after a given seq and stops after unsubscribe', async () => {
    const thread = await createThread();
    const post = (text: string) => send('POST', `/api/threads/${thread.id}/messages`, { text });
    await post('one');
    await post('two');

    const { socket, received } = await connect();
    socket.send(JSON.stringify({ type: 'subscribe', threadId: thread.id, after: 1 }));
    await vi.waitFor(() => expect(eventSeqs(received)).toEqual([2]));

    socket.send(JSON.stringify({ type: 'unsubscribe', threadId: thread.id }));
    await new Promise((resolve) => setTimeout(resolve, 50));
    await post('three');
    await new Promise((resolve) => setTimeout(resolve, 100));
    expect(eventSeqs(received)).toEqual([2]);
  });

  it('reports unknown threads and invalid messages as error messages', async () => {
    const { socket, received } = await connect();
    socket.send(JSON.stringify({ type: 'subscribe', threadId: 'nope', after: 0 }));
    socket.send('not json');
    await vi.waitFor(() => expect(received).toHaveLength(2));
    expect(received[0]).toEqual({ type: 'error', message: 'Unknown thread nope' });
    expect(received[1]).toEqual(expect.objectContaining({ type: 'error' }));
  });
});
