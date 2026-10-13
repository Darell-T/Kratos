import type { ChildProcessByStdio } from 'node:child_process';
import { createInterface } from 'node:readline';
import type { Readable, Writable } from 'node:stream';
import { spawn } from 'cross-spawn';
import { CodexExitedError, CodexNotFoundError, CodexRequestError, CodexTimeoutError } from './errors';
import type { InitializeCapabilities, InitializeResponse } from './generated';
import type {
  NotificationMethod,
  NotificationParams,
  RequestMethod,
  RequestParams,
  RequestResult,
  ServerRequestMethod,
  ServerRequestParams,
  ServerRequestResult,
} from './protocol';
import { INTERNAL_ERROR, METHOD_NOT_FOUND, decode, encode } from './wire';
import type { WireMessage } from './wire';

export interface CodexOptions {
  command?: string;
  args?: readonly string[];
  cwd?: string;
  codexHome?: string;
  clientInfo?: { name: string; title: string; version: string };
  capabilities?: InitializeCapabilities;
  requestTimeoutMs?: number;
  closeTimeoutMs?: number;
}

export interface CodexClient {
  readonly info: InitializeResponse;
  request<M extends RequestMethod>(
    method: M,
    params: RequestParams<M>,
    options?: { timeoutMs: number },
  ): Promise<RequestResult<M>>;
  onNotification<M extends NotificationMethod>(method: M, handler: (params: NotificationParams<M>) => void): () => void;
  onServerRequest<M extends ServerRequestMethod>(
    method: M,
    handler: (
      params: ServerRequestParams<M>,
      id: string | number,
    ) => ServerRequestResult<M> | Promise<ServerRequestResult<M>>,
  ): () => void;
  onExit(handler: (code: number | null) => void): () => void;
  close(): Promise<void>;
}

const DEFAULT_CLIENT_INFO = { name: 'kratos', title: 'Kratos', version: '0.1.0' };

type CodexProcess = ChildProcessByStdio<Writable, Readable, null>;
type PendingRequest = { resolve: (result: unknown) => void; reject: (error: Error) => void; timer?: NodeJS.Timeout };

function connectionTimeouts(options: CodexOptions) {
  for (const timeout of [options.requestTimeoutMs, options.closeTimeoutMs]) {
    if (timeout !== undefined && (!Number.isFinite(timeout) || timeout <= 0))
      throw new Error('Codex timeouts must be positive');
  }
  return { request: options.requestTimeoutMs ?? 30000, close: options.closeTimeoutMs ?? 5000 };
}

export async function connectCodex(options: CodexOptions = {}): Promise<CodexClient> {
  const timeouts = connectionTimeouts(options);
  const command = options.command ?? process.env.CODEX_EXE ?? 'codex';
  const child = spawn(command, options.args ?? ['app-server'], {
    cwd: options.cwd,
    windowsHide: true,
    env: options.codexHome === undefined ? process.env : { ...process.env, CODEX_HOME: options.codexHome },
    stdio: ['pipe', 'pipe', 'inherit'],
  });
  const connection = new CodexConnection(child, command, timeouts.request, timeouts.close);

  try {
    connection.info = parseInitializeResponse(
      await connection.request('initialize', {
        clientInfo: options.clientInfo ?? DEFAULT_CLIENT_INFO,
        capabilities: options.capabilities ?? null,
      }),
    );
    connection.notify('initialized');
  } catch (error) {
    await connection.close();
    throw error;
  }
  return connection;
}

function parseInitializeResponse(value: unknown): InitializeResponse {
  if (typeof value !== 'object' || value === null) throw new Error('Invalid Codex initialize response');
  const stringField = (key: keyof InitializeResponse): string => {
    const field: unknown = Reflect.get(value, key);
    if (typeof field !== 'string') throw new Error('Invalid Codex initialize response: ' + key);
    return field;
  };
  return {
    userAgent: stringField('userAgent'),
    codexHome: stringField('codexHome'),
    platformFamily: stringField('platformFamily'),
    platformOs: stringField('platformOs'),
  };
}

class CodexConnection implements CodexClient {
  // connectCodex sets this right after the handshake, before it hands the client out.
  info!: InitializeResponse;

  private nextId = 1;
  private readonly pending = new Map<number, PendingRequest>();
  private readonly notificationHandlers = new Map<string, Set<(params: unknown) => void>>();
  private readonly serverRequestHandlers = new Map<string, (params: unknown, id: string | number) => unknown>();
  private readonly unansweredServerRequests = new Set<string | number>();
  private readonly exitHandlers = new Set<(code: number | null) => void>();
  private ended: CodexExitedError | CodexNotFoundError | undefined;
  private exitCode: number | null = null;
  private closePromise: Promise<void> | undefined;
  private processClosed = false;

  constructor(
    private readonly child: CodexProcess,
    command: string,
    private readonly requestTimeoutMs: number,
    private readonly closeTimeoutMs: number,
  ) {
    createInterface({ input: child.stdout }).on('line', (line) => {
      const message = decode(line);
      if (message) this.dispatch(message);
    });

    // A broken input pipe makes the connection unusable even if the process has not exited yet.
    child.stdin.on('error', () => {
      this.handleEnd(new CodexExitedError(null));
      stopProcessTree(child);
    });
    child.on('error', (error: NodeJS.ErrnoException) => {
      this.handleEnd(error.code === 'ENOENT' ? new CodexNotFoundError(command) : new CodexExitedError(null));
    });
    // 'close' fires after stdout is drained, so no output is lost before pending requests are rejected.
    child.on('close', (code) => {
      this.processClosed = true;
      this.handleEnd(new CodexExitedError(code));
    });
  }

  request<M extends RequestMethod>(
    method: M,
    params: RequestParams<M>,
    options?: { timeoutMs: number },
  ): Promise<RequestResult<M>> {
    if (this.ended) return Promise.reject(this.ended);
    if (this.closePromise) return Promise.reject(new CodexExitedError(this.exitCode));

    const timeoutMs = options?.timeoutMs ?? this.requestTimeoutMs;
    if (!Number.isFinite(timeoutMs) || timeoutMs <= 0)
      return Promise.reject(new Error('Codex timeout must be positive'));
    let message: string;
    try {
      message = encode({ id: this.nextId, method, params });
    } catch (error) {
      return Promise.reject(error instanceof Error ? error : new Error(String(error)));
    }

    const id = this.nextId++;
    const result = new Promise<unknown>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new CodexTimeoutError(method, timeoutMs));
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timer });
    });
    this.write(message);
    return result as Promise<RequestResult<M>>;
  }

  notify(method: string): void {
    this.write(encode({ method }));
  }

  onNotification<M extends NotificationMethod>(
    method: M,
    handler: (params: NotificationParams<M>) => void,
  ): () => void {
    const handlers = this.notificationHandlers.get(method) ?? new Set();
    const untypedHandler = handler as (params: unknown) => void;
    handlers.add(untypedHandler);
    this.notificationHandlers.set(method, handlers);
    return () => handlers.delete(untypedHandler);
  }

  onServerRequest<M extends ServerRequestMethod>(
    method: M,
    handler: (
      params: ServerRequestParams<M>,
      id: string | number,
    ) => ServerRequestResult<M> | Promise<ServerRequestResult<M>>,
  ): () => void {
    const untypedHandler = handler as (params: unknown, id: string | number) => unknown;
    this.serverRequestHandlers.set(method, untypedHandler);
    return () => {
      // A newer registration replaced this one, so it is no longer ours to remove.
      if (this.serverRequestHandlers.get(method) === untypedHandler) this.serverRequestHandlers.delete(method);
    };
  }

  onExit(handler: (code: number | null) => void): () => void {
    if (this.ended) {
      runHandler('exit', () => handler(this.exitCode));
    } else {
      this.exitHandlers.add(handler);
    }
    return () => this.exitHandlers.delete(handler);
  }

  async close(): Promise<void> {
    if (this.closePromise) return this.closePromise;
    if (this.processClosed) return;
    this.closePromise = new Promise<void>((resolve, reject) => {
      const complete = () => {
        clearTimeout(timer);
        resolve();
      };
      const timer = setTimeout(() => {
        this.child.off('close', complete);
        this.child.kill('SIGKILL');
        reject(new CodexTimeoutError('close', this.closeTimeoutMs));
      }, this.closeTimeoutMs);
      this.child.once('close', complete);
      stopProcessTree(this.child);
    });
    return this.closePromise;
  }

  private write(text: string): void {
    this.child.stdin.write(text);
  }

  private dispatch(message: WireMessage): void {
    switch (message.kind) {
      case 'response':
      case 'error':
        this.settle(message);
        break;
      case 'notification':
        if (message.method === 'serverRequest/resolved') {
          const id = resolvedRequestId(message.params);
          if (id !== undefined) this.unansweredServerRequests.delete(id);
        }
        for (const handler of this.notificationHandlers.get(message.method) ?? []) {
          runHandler(message.method, () => handler(message.params));
        }
        break;
      case 'serverRequest':
        void this.answerServerRequest(message.id, message.method, message.params);
        break;
    }
  }

  private settle(message: Extract<WireMessage, { kind: 'response' | 'error' }>): void {
    if (typeof message.id !== 'number') return;
    const request = this.pending.get(message.id);
    if (!request) return;

    this.pending.delete(message.id);
    clearTimeout(request.timer);
    if (message.kind === 'response') request.resolve(message.result);
    else request.reject(new CodexRequestError(message.code, message.message));
  }

  private async answerServerRequest(id: string | number, method: string, params: unknown): Promise<void> {
    const handler = this.serverRequestHandlers.get(method);
    if (!handler) {
      this.write(encode({ id, error: { code: METHOD_NOT_FOUND, message: `Method not handled by Kratos: ${method}` } }));
      return;
    }
    this.unansweredServerRequests.add(id);
    try {
      const result = await handler(params, id);
      if (this.unansweredServerRequests.delete(id)) this.write(encode({ id, result: result ?? null }));
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      if (this.unansweredServerRequests.delete(id))
        this.write(encode({ id, error: { code: INTERNAL_ERROR, message } }));
    }
  }

  private handleEnd(reason: CodexExitedError | CodexNotFoundError): void {
    if (this.ended) return;
    this.ended = reason;
    if (reason instanceof CodexExitedError) this.exitCode = reason.exitCode;

    for (const request of this.pending.values()) {
      clearTimeout(request.timer);
      request.reject(reason);
    }
    this.pending.clear();
    // Exit subscribers may settle approval promises; their answers must not go to a closed process.
    this.unansweredServerRequests.clear();

    const handlers = [...this.exitHandlers];
    this.exitHandlers.clear();
    for (const handler of handlers) runHandler('exit', () => handler(this.exitCode));
  }
}

function resolvedRequestId(params: unknown): string | number | undefined {
  if (typeof params !== 'object' || params === null || !('requestId' in params)) return;
  const id = params.requestId;
  return typeof id === 'string' || typeof id === 'number' ? id : undefined;
}

// One misbehaving subscriber must not stop the others or take down the process that reads Codex's output.
function runHandler(what: string, handler: () => void): void {
  try {
    handler();
  } catch (error) {
    console.error(`Codex ${what} handler threw`, error);
  }
}

// The npm launcher on Windows starts child processes of its own, so killing only our direct child leaves Codex running.
function stopProcessTree(child: CodexProcess): void {
  if (child.exitCode !== null) return;
  if (process.platform !== 'win32' || child.pid === undefined) {
    child.kill('SIGTERM');
    return;
  }
  const killer = spawn('taskkill', ['/pid', String(child.pid), '/T', '/F'], { stdio: 'ignore', windowsHide: true });
  const fallback = () => {
    if (child.exitCode === null) child.kill('SIGKILL');
  };
  killer.on('error', fallback);
  killer.on('close', (code) => {
    if (code !== 0) fallback();
  });
}
