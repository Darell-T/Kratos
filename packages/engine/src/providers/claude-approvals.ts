import { randomUUID } from 'node:crypto';
import { z } from 'zod';
import type {
  CanUseTool,
  ElicitationRequest,
  ElicitationResult,
  OnElicitation,
  PermissionResult,
  PermissionUpdate,
} from '@anthropic-ai/claude-agent-sdk';

export type Permission = 'askMe' | 'approveForMe' | 'fullAccess';
export interface ClaudeApproval {
  requestId: string;
  threadId: string;
  turnId: string | null;
  toolName: string;
  input: Record<string, unknown>;
  title: string | null;
  description: string | null;
  reason: string | null;
  blockedPath: string | null;
  canRememberForSession: boolean;
}
export type ClaudeApprovalDecision = 'allow' | 'allowForSession' | 'deny';
export interface ClaudeQuestion {
  requestId: string;
  threadId: string;
  turnId: string | null;
  questions: unknown[];
}
export interface ClaudeElicitation {
  requestId: string;
  threadId: string;
  request: ElicitationRequest;
}
export type ClaudeRequestEvent =
  | { kind: 'approvals.changed'; threadId: string; approvals: ClaudeApproval[] }
  | { kind: 'question.requested'; question: ClaudeQuestion }
  | { kind: 'elicitation.requested'; elicitation: ClaudeElicitation };

type Request =
  | {
      kind: 'approval';
      details: ClaudeApproval;
      sessionRules: PermissionUpdate[];
      resolve: (result: PermissionResult) => void;
    }
  | {
      kind: 'question';
      details: ClaudeQuestion;
      input: Record<string, unknown>;
      resolve: (result: PermissionResult) => void;
    }
  | { kind: 'elicitation'; details: ClaudeElicitation; resolve: (result: ElicitationResult) => void };
type ToolOptions = Parameters<CanUseTool>[2];

const declined: PermissionResult = { behavior: 'deny', message: 'The user declined this action' };
const unavailable: PermissionResult = { behavior: 'deny', message: 'This Kratos thread is not available' };
const questionSchema = z.object({ questions: z.array(z.unknown()) });

export function createClaudeApprovals(
  threadFor: (threadId: string) => { permissions: Permission; turnId: string | null } | undefined,
  emit: (event: ClaudeRequestEvent) => void,
) {
  const requests = new Map<string, Request>();
  const owned = (threadId: string) => [...requests.values()].filter((request) => request.details.threadId === threadId);
  const approvalsWaiting = (threadId: string): ClaudeApproval[] =>
    structuredClone(owned(threadId).flatMap((request) => (request.kind === 'approval' ? [request.details] : [])));
  const approvalsChanged = (threadId: string) =>
    emit({ kind: 'approvals.changed', threadId, approvals: approvalsWaiting(threadId) });

  function take(requestId: string): Request | undefined {
    const request = requests.get(requestId);
    if (!request) return undefined;
    requests.delete(requestId);
    if (request.kind === 'approval') approvalsChanged(request.details.threadId);
    return request;
  }

  function cancel(requestId: string) {
    const request = take(requestId);
    if (request?.kind === 'elicitation') request.resolve({ action: 'cancel' });
    else request?.resolve(declined);
  }

  function track(request: Request, signal: AbortSignal) {
    const { requestId } = request.details;
    requests.set(requestId, request);
    signal.addEventListener('abort', () => {
      if (requests.get(requestId) === request) cancel(requestId);
    });
  }

  function ask(
    threadId: string,
    turnId: string | null,
    toolName: string,
    input: Record<string, unknown>,
    options: ToolOptions,
  ) {
    const requestId = options.toolUseID || randomUUID();
    const question = toolName === 'AskUserQuestion' ? questionSchema.safeParse(input) : undefined;
    return new Promise<PermissionResult>((resolve) => {
      if (question?.success) {
        const details: ClaudeQuestion = { requestId, threadId, turnId, questions: question.data.questions };
        track({ kind: 'question', details, input, resolve }, options.signal);
        emit({ kind: 'question.requested', question: structuredClone(details) });
        return;
      }
      const sessionRules = options.suggestions?.filter((rule) => rule.destination === 'session') ?? [];
      const details: ClaudeApproval = {
        requestId,
        threadId,
        turnId,
        toolName,
        input,
        title: options.title ?? null,
        description: options.description ?? null,
        reason: options.decisionReason ?? null,
        blockedPath: options.blockedPath ?? null,
        canRememberForSession: sessionRules.length > 0 && !options.suppressAlwaysAllowRule,
      };
      track({ kind: 'approval', details, sessionRules, resolve }, options.signal);
      approvalsChanged(threadId);
    });
  }

  const canUseTool =
    (threadId: string): CanUseTool =>
    async (toolName, input, options) => {
      const thread = threadFor(threadId);
      if (!thread || options.signal.aborted) return unavailable;
      if (thread.permissions === 'fullAccess') return { behavior: 'allow', updatedInput: input };
      return ask(threadId, thread.turnId, toolName, input, options);
    };

  const onElicitation =
    (threadId: string): OnElicitation =>
    async (request, { signal }) => {
      if (!threadFor(threadId) || signal.aborted) return { action: 'cancel' };
      return new Promise<ElicitationResult>((resolve) => {
        const details: ClaudeElicitation = { requestId: randomUUID(), threadId, request };
        track({ kind: 'elicitation', details, resolve }, signal);
        emit({ kind: 'elicitation.requested', elicitation: structuredClone(details) });
      });
    };

  function decideApproval(requestId: string, decision: ClaudeApprovalDecision): boolean {
    const request = requests.get(requestId);
    if (request?.kind !== 'approval') return false;
    if (decision === 'allowForSession' && !request.details.canRememberForSession) return false;
    take(requestId);
    const updatedInput = request.details.input;
    if (decision === 'deny') request.resolve(declined);
    else if (decision === 'allow') request.resolve({ behavior: 'allow', updatedInput });
    else request.resolve({ behavior: 'allow', updatedInput, updatedPermissions: request.sessionRules });
    return true;
  }

  function answerQuestion(requestId: string, answers: Record<string, string>): boolean {
    const request = requests.get(requestId);
    if (request?.kind !== 'question') return false;
    take(requestId);
    request.resolve({ behavior: 'allow', updatedInput: { ...request.input, answers } });
    return true;
  }

  function answerElicitation(requestId: string, result: ElicitationResult): boolean {
    const request = requests.get(requestId);
    if (request?.kind !== 'elicitation') return false;
    take(requestId);
    request.resolve(result);
    return true;
  }

  function clearRequests(threadId?: string) {
    for (const [requestId, request] of requests)
      if (threadId === undefined || request.details.threadId === threadId) cancel(requestId);
  }

  return {
    canUseTool,
    onElicitation,
    clearRequests,
    pendingInput: (threadId: string) => owned(threadId).length > 0,
    approvalsWaiting,
    questionsWaiting: (threadId: string): ClaudeQuestion[] =>
      structuredClone(owned(threadId).flatMap((request) => (request.kind === 'question' ? [request.details] : []))),
    elicitationsWaiting: (threadId: string): ClaudeElicitation[] =>
      structuredClone(owned(threadId).flatMap((request) => (request.kind === 'elicitation' ? [request.details] : []))),
    decideApproval,
    answerQuestion,
    answerElicitation,
  };
}
