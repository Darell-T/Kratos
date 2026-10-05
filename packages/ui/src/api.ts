import { z } from 'zod';
import {
  serverMessageSchema,
  threadEventSchema,
  threadSchema,
  workspaceSchema,
  type ClientMessage,
  type CreateThreadBody,
  type CreateWorkspaceBody,
  type ThreadEvent,
} from '@kratos/protocol';

const errorBodySchema = z.object({ error: z.string() });

async function request<S extends z.ZodType>(
  schema: S,
  method: 'GET' | 'POST',
  path: string,
  body?: unknown,
): Promise<z.output<S>> {
  const response = await fetch(path, {
    method,
    ...(body === undefined ? {} : { headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) }),
  });
  const payload: unknown = await response.json().catch(() => null);
  if (!response.ok) {
    const parsed = errorBodySchema.safeParse(payload);
    throw new Error(parsed.success ? parsed.data.error : `Request failed with status ${response.status}`);
  }
  return schema.parse(payload);
}

export const listWorkspaces = () => request(z.array(workspaceSchema), 'GET', '/api/workspaces');

export const listThreads = (workspaceId: string) =>
  request(z.array(threadSchema), 'GET', `/api/threads?workspaceId=${encodeURIComponent(workspaceId)}`);

export const createWorkspace = (body: CreateWorkspaceBody) => request(workspaceSchema, 'POST', '/api/workspaces', body);

export const createThread = (body: CreateThreadBody) => request(threadSchema, 'POST', '/api/threads', body);

export const postMessage = (threadId: string, text: string) =>
  request(threadEventSchema, 'POST', `/api/threads/${encodeURIComponent(threadId)}/messages`, { text });

const reconnectDelayMs = 1000;

interface Subscription {
  lastSeq: number;
  listener: (event: ThreadEvent) => void;
}

// One socket carries every thread subscription. After a drop it resubscribes each thread from the last seq it saw.
class EventStream {
  private socket: WebSocket | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | undefined;
  private readonly subscriptions = new Map<string, Subscription>();

  subscribe(threadId: string, listener: Subscription['listener']): () => void {
    const subscription: Subscription = { lastSeq: 0, listener };
    this.subscriptions.set(threadId, subscription);
    if (this.socket) {
      this.send({ type: 'subscribe', threadId, after: 0 });
    } else if (this.reconnectTimer === undefined) {
      this.connect();
    }
    return () => {
      if (this.subscriptions.get(threadId) !== subscription) return;
      this.subscriptions.delete(threadId);
      this.send({ type: 'unsubscribe', threadId });
    };
  }

  private connect(): void {
    this.reconnectTimer = undefined;
    const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const socket = new WebSocket(`${scheme}//${location.host}/api/events`);
    this.socket = socket;
    socket.addEventListener('open', () => {
      for (const [threadId, { lastSeq }] of this.subscriptions) {
        this.send({ type: 'subscribe', threadId, after: lastSeq });
      }
    });
    socket.addEventListener('message', (message: MessageEvent) => this.receive(message.data));
    socket.addEventListener('close', () => {
      this.socket = null;
      if (this.subscriptions.size > 0) {
        this.reconnectTimer = setTimeout(() => this.connect(), reconnectDelayMs);
      }
    });
  }

  private send(message: ClientMessage): void {
    if (this.socket?.readyState === WebSocket.OPEN) this.socket.send(JSON.stringify(message));
  }

  private receive(data: unknown): void {
    if (typeof data !== 'string') return;
    const message = serverMessageSchema.safeParse(JSON.parse(data));
    if (!message.success) return;
    if (message.data.type === 'error') {
      console.warn(`event stream: ${message.data.message}`);
      return;
    }
    const { event } = message.data;
    const subscription = this.subscriptions.get(event.threadId);
    if (!subscription || event.seq <= subscription.lastSeq) return;
    subscription.lastSeq = event.seq;
    subscription.listener(event);
  }
}

const stream = new EventStream();

export const subscribeToThread = (threadId: string, listener: Subscription['listener']) =>
  stream.subscribe(threadId, listener);
