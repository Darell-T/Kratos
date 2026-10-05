import type { ChildProcessByStdio } from 'node:child_process';
import { createInterface } from 'node:readline';
import type { Readable, Writable } from 'node:stream';
import { spawn } from 'cross-spawn';
import { CodexExitedError, CodexNotFoundError, CodexRequestError } from './errors';
import type { InitializeResponse } from './generated';
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
}

export interface CodexClient {
  readonly info: InitializeResponse;
  request<M extends RequestMethod>(method: M, params: RequestParams<M>): Promise<RequestResult<M>>;
  onNotification<M extends NotificationMethod>(method: M, handler: (params: NotificationParams<M>) => void): () => void;
  onServerRequest<M extends ServerRequestMethod>(
    method: M,
    handler: (params: ServerRequestParams<M>) => ServerRequestResult<M> | Promise<ServerRequestResult<M>>,
  ): () => void;
  onExit(handler: (code: number | null) => void): () => void;
  close(): Promise<void>;
}

const DEFAULT_CLIENT_INFO = { name: 'kratos', title: 'Kratos', version: '0.1.0' };

type CodexProcess = ChildProcessByStdio<Writable, Readable, null>;
type PendingRequest = { resolve: (result: unknown) => void; reject: (error: Error) => void };

export async function connectCodex(options: CodexOptions = {}): Promise<CodexClient> {
  const command = options.command ?? process.env.CODEX_EXE ?? 'codex';
  const child = spawn(command, options.args ?? ['app-server'], {
    cwd: options.cwd,
    env: options.codexHome === undefined ? process.env : { ...process.env, CODEX_HOME: options.codexHome },
    stdio: ['pipe', 'pipe', 'inherit'],
  });
  const connection = new CodexConnection(child, command);

  try {
    connection.info = await connection.request('initialize', {
      clientInfo: options.clientInfo ?? DEFAULT_CLIENT_INFO,
      capabilities: null,
    });
    connection.notify('initialized');
  } catch (error) {
    await connection.close();
    throw error;
  }
  return connection;
}

class CodexConnection implements CodexClient {
  // connectCodex sets this right after the handshake, before it hands the client out.
  info!: InitializeResponse;

  private nextId = 1;
  private readonly pending = new Map<number, PendingRequest>();
  private readonly notificationHandlers = new Map<string, Set<(params: unknown) => void>>();
  private readonly serverRequestHandlers = new Map<string, (params: unknown) => unknown>();
  private readonly exitHandlers = new Set<(code: number | null) => void>();
  private ended: CodexExitedError | CodexNotFoundError | undefined;
  private exitCode: number | null = null;

  constructor(
    private readonly child: CodexProcess,
    command: string,
  ) {
    createInterface({ input: child.stdout }).on('line', (line) => {
      const message = decode(line);
      if (message) this.dispatch(message);
    });

    // Writing to a dead process emits an error here. The exit handling below reports the real cause.
    child.stdin.on('error', () => undefined);
    child.on('error', (error: NodeJS.ErrnoException) => {
      this.handleEnd(error.code === 'ENOENT' ? new CodexNotFoundError(command) : new CodexExitedError(null));
    });
    // 'close' fires after stdout is drained, so no output is lost before pending requests are rejected.
    child.on('close', (code) => this.handleEnd(new CodexExitedError(code)));
  }

  request<M extends RequestMethod>(method: M, params: RequestParams<M>): Promise<RequestResult<M>> {
    if (this.ended) return Promise.reject(this.ended);

    const id = this.nextId++;
    const result = new Promise<unknown>((resolve, reject) => this.pending.set(id, { resolve, reject }));
    this.write(encode({ id, method, params }));
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
    handler: (params: ServerRequestParams<M>) => ServerRequestResult<M> | Promise<ServerRequestResult<M>>,
  ): () => void {
    const untypedHandler = handler as (params: unknown) => unknown;
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
    if (!this.ended) stopProcessTree(this.child);
    await new Promise<void>((resolve) => this.onExit(() => resolve()));
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
    if (message.kind === 'response') request.resolve(message.result);
    else request.reject(new CodexRequestError(message.code, message.message));
  }

  private async answerServerRequest(id: string | number, method: string, params: unknown): Promise<void> {
    const handler = this.serverRequestHandlers.get(method);
    if (!handler) {
      this.write(encode({ id, error: { code: METHOD_NOT_FOUND, message: `Method not handled by Kratos: ${method}` } }));
      return;
    }
    try {
      const result = await handler(params);
      this.write(encode({ id, result: result ?? null }));
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      this.write(encode({ id, error: { code: INTERNAL_ERROR, message } }));
    }
  }

  private handleEnd(reason: CodexExitedError | CodexNotFoundError): void {
    if (this.ended) return;
    this.ended = reason;
    if (reason instanceof CodexExitedError) this.exitCode = reason.exitCode;

    for (const request of this.pending.values()) request.reject(reason);
    this.pending.clear();

    const handlers = [...this.exitHandlers];
    this.exitHandlers.clear();
    for (const handler of handlers) runHandler('exit', () => handler(this.exitCode));
  }
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
  if (process.platform !== 'win32' || child.pid === undefined) {
    child.kill('SIGTERM');
    return;
  }
  spawn('taskkill', ['/pid', String(child.pid), '/T', '/F'], { stdio: 'ignore' }).on('error', () => child.kill());
}
