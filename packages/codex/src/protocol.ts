import type { ClientRequest, ServerNotification, ServerRequest } from './generated';
import type { ClientResponses, ServerRequestResponses } from './generated/responses';

export type RequestMethod = ClientRequest['method'];
export type RequestParams<M extends RequestMethod> = Extract<ClientRequest, { method: M }>['params'];
export type RequestResult<M extends RequestMethod> = M extends keyof ClientResponses ? ClientResponses[M] : unknown;

export type NotificationMethod = ServerNotification['method'];
export type NotificationParams<M extends NotificationMethod> = Extract<ServerNotification, { method: M }>['params'];

export type ServerRequestMethod = ServerRequest['method'];
export type ServerRequestParams<M extends ServerRequestMethod> = Extract<ServerRequest, { method: M }>['params'];
export type ServerRequestResult<M extends ServerRequestMethod> = M extends keyof ServerRequestResponses
  ? ServerRequestResponses[M]
  : unknown;
