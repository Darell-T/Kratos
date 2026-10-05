import type { RequestId } from './generated';

export type WireMessage =
  | { kind: 'response'; id: RequestId; result: unknown }
  | { kind: 'error'; id: RequestId; code: number; message: string }
  | { kind: 'notification'; method: string; params: unknown }
  | { kind: 'serverRequest'; id: RequestId; method: string; params: unknown };

export const METHOD_NOT_FOUND = -32601;
export const INTERNAL_ERROR = -32603;

type Outgoing =
  | { id: RequestId; method: string; params: unknown }
  | { method: string; params?: unknown }
  | { id: RequestId; result: unknown }
  | { id: RequestId; error: { code: number; message: string } };

export function encode(message: Outgoing): string {
  return `${JSON.stringify(message)}\n`;
}

// Codex logs to stderr, but a stray non-JSON line on stdout must not crash the connection.
export function decode(line: string): WireMessage | undefined {
  let parsed: unknown;
  try {
    parsed = JSON.parse(line);
  } catch {
    return undefined;
  }
  if (typeof parsed !== 'object' || parsed === null) return undefined;

  const { id, method, params, result, error } = parsed as Record<string, unknown>;
  const hasId = typeof id === 'string' || typeof id === 'number';

  if (typeof method === 'string') {
    return hasId ? { kind: 'serverRequest', id, method, params } : { kind: 'notification', method, params };
  }
  if (!hasId) return undefined;
  return typeof error === 'object' && error !== null ? decodeError(id, error) : { kind: 'response', id, result };
}

function decodeError(id: RequestId, error: object): WireMessage {
  const { code, message } = error as Record<string, unknown>;
  return {
    kind: 'error',
    id,
    code: typeof code === 'number' ? code : INTERNAL_ERROR,
    message: typeof message === 'string' ? message : JSON.stringify(error),
  };
}
