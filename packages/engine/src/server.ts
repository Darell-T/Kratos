import { readFile, stat } from 'node:fs/promises';
import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';
import type { AddressInfo } from 'node:net';
import { extname, isAbsolute, join, relative, resolve } from 'node:path';
import { WebSocketServer, type RawData, type WebSocket } from 'ws';
import { z } from 'zod';
import {
  clientMessageSchema,
  createThreadBody,
  createWorkspaceBody,
  postMessageBody,
  type ServerMessage,
} from '@kratos/protocol';
import type { Hub } from './hub';
import type { Store } from './store';

const host = '127.0.0.1';
const eventsPath = '/api/events';
const maxBodyBytes = 1024 * 1024;
const maxSocketMessageBytes = 64 * 1024;

export interface ServerOptions {
  store: Store;
  hub: Hub;
  uiDist: string;
  port: number;
}

export interface RunningServer {
  port: number;
  close(): Promise<void>;
}

class HttpError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
  }
}

interface Reply {
  status: number;
  body: unknown;
}

const ok = (body: unknown): Reply => ({ status: 200, body });
const created = (body: unknown): Reply => ({ status: 201, body });

interface Context {
  params: Record<string, string>;
  query: URLSearchParams;
  readBody: () => Promise<unknown>;
}

interface Route {
  method: string;
  segments: string[];
  handle: (context: Context) => Reply | Promise<Reply>;
}

const route = (method: string, pattern: string, handle: Route['handle']): Route => ({
  method,
  segments: pattern.split('/').filter(Boolean),
  handle,
});

function parseWith<S extends z.ZodType>(schema: S, value: unknown): z.output<S> {
  const result = schema.safeParse(value);
  if (!result.success) throw new HttpError(400, z.prettifyError(result.error));
  return result.data;
}

async function readJson(req: IncomingMessage): Promise<unknown> {
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const chunk of req as AsyncIterable<Buffer>) {
    size += chunk.length;
    if (size > maxBodyBytes) throw new HttpError(413, 'Request body is too large');
    chunks.push(chunk);
  }
  try {
    return JSON.parse(Buffer.concat(chunks).toString('utf8'));
  } catch {
    throw new HttpError(400, 'Request body must be valid JSON');
  }
}

async function statOrNull(path: string) {
  return stat(path).catch(() => null);
}

async function assertDirectory(path: string): Promise<void> {
  const info = isAbsolute(path) ? await statOrNull(path) : null;
  if (!info?.isDirectory()) throw new HttpError(400, `Path is not an existing absolute directory: ${path}`);
}

function buildRoutes(store: Store, hub: Hub): Route[] {
  const requireThread = (id: string) => {
    const thread = store.getThread(id);
    if (!thread) throw new HttpError(404, `Unknown thread ${id}`);
    return thread;
  };
  const requireWorkspace = (id: string) => {
    const workspace = store.getWorkspace(id);
    if (!workspace) throw new HttpError(404, `Unknown workspace ${id}`);
    return workspace;
  };

  return [
    route('GET', '/api/health', () => ok({ ok: true })),
    route('GET', '/api/workspaces', () => ok(store.listWorkspaces())),
    route('POST', '/api/workspaces', async ({ readBody }) => {
      const body = parseWith(createWorkspaceBody, await readBody());
      await assertDirectory(body.path);
      return created(store.createWorkspace(body));
    }),
    route('GET', '/api/threads', ({ query }) => {
      const { workspaceId } = parseWith(z.object({ workspaceId: z.string().min(1) }), Object.fromEntries(query));
      return ok(store.listThreads(requireWorkspace(workspaceId).id));
    }),
    route('POST', '/api/threads', async ({ readBody }) => {
      const body = parseWith(createThreadBody, await readBody());
      requireWorkspace(body.workspaceId);
      return created(store.createThread(body));
    }),
    route('POST', '/api/threads/:id/messages', async ({ params, readBody }) => {
      const thread = requireThread(params.id ?? '');
      const { text } = parseWith(postMessageBody, await readBody());
      // No await between append and publish: a subscriber replaying the log sees the event exactly once.
      const event = store.appendEvent(thread.id, { kind: 'user.message', text });
      hub.publish(event);
      return created(event);
    }),
  ];
}

function matchRoute(routes: Route[], method: string, pathname: string) {
  const parts = pathname.split('/').filter(Boolean);
  for (const candidate of routes) {
    if (candidate.method !== method || candidate.segments.length !== parts.length) continue;
    const params: Record<string, string> = {};
    const matches = candidate.segments.every((segment, index) => {
      const part = parts[index] ?? '';
      if (!segment.startsWith(':')) return segment === part;
      params[segment.slice(1)] = decodePathPart(part);
      return true;
    });
    if (matches) return { candidate, params };
  }
  return undefined;
}

function decodePathPart(part: string): string {
  try {
    return decodeURIComponent(part);
  } catch {
    throw new HttpError(400, 'Malformed URL path');
  }
}

function sendJson(res: ServerResponse, status: number, body: unknown): void {
  const payload = JSON.stringify(body);
  res.writeHead(status, {
    'content-type': 'application/json; charset=utf-8',
    'content-length': Buffer.byteLength(payload),
  });
  res.end(payload);
}

function sendText(res: ServerResponse, status: number, text: string): void {
  res.writeHead(status, { 'content-type': 'text/plain; charset=utf-8' });
  res.end(text);
}

const contentTypes: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
  '.woff2': 'font/woff2',
  '.map': 'application/json; charset=utf-8',
};

const missingUiNote =
  'The engine is running but there is no UI build yet.\n' +
  'Run `npm run build` and restart, or run `npm run dev` and open http://localhost:5173.\n';

async function isFile(path: string): Promise<boolean> {
  return (await statOrNull(path))?.isFile() ?? false;
}

async function serveUi(root: string, pathname: string, res: ServerResponse): Promise<void> {
  const requested = resolve(root, `.${decodePathPart(pathname)}`);
  const inside = relative(root, requested);
  if (inside.startsWith('..') || isAbsolute(inside)) throw new HttpError(404, 'Not found');

  let file = requested;
  if (!(await isFile(file))) {
    // A missing asset must not be answered with index.html, or the browser reports a confusing MIME error.
    if (extname(pathname)) throw new HttpError(404, 'Not found');
    file = join(root, 'index.html');
  }
  if (!(await isFile(file))) {
    sendText(res, 503, missingUiNote);
    return;
  }
  const data = await readFile(file);
  res.writeHead(200, {
    'content-type': contentTypes[extname(file)] ?? 'application/octet-stream',
    'content-length': data.length,
  });
  res.end(data);
}

async function dispatch(routes: Route[], uiDist: string, req: IncomingMessage, res: ServerResponse): Promise<void> {
  const method = req.method ?? 'GET';
  const url = new URL(req.url ?? '/', 'http://localhost');
  if (url.pathname === '/api' || url.pathname.startsWith('/api/')) {
    const matched = matchRoute(routes, method, url.pathname);
    if (!matched) throw new HttpError(404, 'Not found');
    const reply = await matched.candidate.handle({
      params: matched.params,
      query: url.searchParams,
      readBody: () => readJson(req),
    });
    sendJson(res, reply.status, reply.body);
  } else if (method === 'GET' || method === 'HEAD') {
    await serveUi(uiDist, url.pathname, res);
  } else {
    throw new HttpError(405, 'Method not allowed');
  }
}

function sendError(res: ServerResponse, error: unknown): void {
  if (res.headersSent) {
    res.destroy();
  } else if (error instanceof HttpError) {
    sendJson(res, error.status, { error: error.message });
  } else {
    console.error(error);
    sendJson(res, 500, { error: 'Internal server error' });
  }
}

const localHostnames = new Set(['localhost', '127.0.0.1', '[::1]']);

function isLocalUrl(url: string): boolean {
  return URL.canParse(url) && localHostnames.has(new URL(url).hostname);
}

// Any website open in a browser can send requests to localhost. Requiring a local Host name stops DNS rebinding,
// and requiring a local Origin (browsers always send one across sites) stops other pages from driving the engine.
function fromLocalPage(req: IncomingMessage): boolean {
  const { host, origin } = req.headers;
  return host !== undefined && isLocalUrl(`http://${host}`) && (origin === undefined || isLocalUrl(origin));
}

async function handleRequest(routes: Route[], uiDist: string, req: IncomingMessage, res: ServerResponse) {
  try {
    if (!fromLocalPage(req)) throw new HttpError(403, 'Forbidden');
    await dispatch(routes, uiDist, req, res);
  } catch (error) {
    sendError(res, error);
  }
}

function parseJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

function rawText(data: RawData): string {
  if (Array.isArray(data)) return Buffer.concat(data).toString('utf8');
  if (data instanceof ArrayBuffer) return Buffer.from(data).toString('utf8');
  return data.toString('utf8');
}

function serveSocket(socket: WebSocket, store: Store, hub: Hub): void {
  const subscriptions = new Map<string, () => void>();
  const send = (message: ServerMessage) => socket.send(JSON.stringify(message));

  const unsubscribe = (threadId: string) => {
    subscriptions.get(threadId)?.();
    subscriptions.delete(threadId);
  };

  const subscribe = (threadId: string, after: number) => {
    if (!store.getThread(threadId)) {
      send({ type: 'error', message: `Unknown thread ${threadId}` });
      return;
    }
    unsubscribe(threadId);
    // Replay and subscribe run in one synchronous step, so no event can land between them or arrive twice.
    for (const event of store.eventsAfter(threadId, after)) send({ type: 'event', event });
    subscriptions.set(
      threadId,
      hub.subscribe(threadId, (event) => send({ type: 'event', event })),
    );
  };

  socket.on('message', (data) => {
    try {
      const message = clientMessageSchema.safeParse(parseJson(rawText(data)));
      if (!message.success) {
        send({ type: 'error', message: z.prettifyError(message.error) });
      } else if (message.data.type === 'subscribe') {
        subscribe(message.data.threadId, message.data.after);
      } else {
        unsubscribe(message.data.threadId);
      }
    } catch (error) {
      console.error(error);
      send({ type: 'error', message: 'Internal server error' });
    }
  });
  socket.on('close', () => {
    for (const stop of subscriptions.values()) stop();
    subscriptions.clear();
  });
  socket.on('error', () => socket.terminate());
}

export async function startServer({ store, hub, uiDist, port }: ServerOptions): Promise<RunningServer> {
  const routes = buildRoutes(store, hub);
  const http = createServer((req, res) => void handleRequest(routes, uiDist, req, res));
  const sockets = new WebSocketServer({ noServer: true, maxPayload: maxSocketMessageBytes });
  sockets.on('connection', (socket) => serveSocket(socket, store, hub));
  http.on('upgrade', (req, socket, head) => {
    if (fromLocalPage(req) && new URL(req.url ?? '/', 'http://localhost').pathname === eventsPath) {
      sockets.handleUpgrade(req, socket, head, (ws) => sockets.emit('connection', ws, req));
    } else {
      socket.destroy();
    }
  });

  await new Promise<void>((resolveListening, reject) => {
    http.once('error', reject);
    http.listen(port, host, () => {
      http.off('error', reject);
      resolveListening();
    });
  });

  return {
    port: (http.address() as AddressInfo).port,
    close: () =>
      new Promise<void>((resolveClosed, reject) => {
        for (const socket of sockets.clients) socket.terminate();
        sockets.close();
        http.close((error) => (error ? reject(error) : resolveClosed()));
        http.closeAllConnections();
      }),
  };
}
