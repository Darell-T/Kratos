import type { CodexClient, RequestId, v2 } from '@kratos/codex';

export type Permission = 'askMe' | 'approveForMe' | 'fullAccess';
export type ApprovalDecision = v2.FileChangeApprovalDecision;

interface ApprovalOwner {
  requestId: RequestId;
  threadId: string;
  turnId: string;
  itemId: string;
}

export interface PendingCommandApproval extends ApprovalOwner {
  kind: 'command';
  command: string | null;
  reason: string | null;
  cwd: string | null;
}

export interface PendingFileApproval extends ApprovalOwner {
  kind: 'file';
  reason: string | null;
  grantRoot: string | null;
  // null means the corresponding item has not arrived yet, rather than "no changes".
  changes: v2.FileUpdateChange[] | null;
}

type Approval = PendingCommandApproval | PendingFileApproval;
interface Pending<T extends Approval> {
  details: T;
  resolve: (response: { decision: ApprovalDecision }) => void;
}

function decide<T extends Approval>(
  pending: Map<RequestId, Pending<T>>,
  requestId: RequestId,
  decision: ApprovalDecision,
): boolean {
  const request = pending.get(requestId);
  if (!request) return false;
  pending.delete(requestId);
  request.resolve({ decision });
  return true;
}

// The request ID answers an approval; the item ID locates the proposed file changes.
const itemKey = (threadId: string, turnId: string, itemId: string) => JSON.stringify([threadId, turnId, itemId]);

export function createCodexApprovals(codex: CodexClient, permissionFor: (threadId: string) => Permission | undefined) {
  const pendingCommands = new Map<RequestId, Pending<PendingCommandApproval>>();
  const pendingFiles = new Map<RequestId, Pending<PendingFileApproval>>();
  const fileItems = new Map<string, { threadId: string; turnId: string; changes: v2.FileUpdateChange[] }>();

  codex.onServerRequest('item/commandExecution/requestApproval', (params, requestId) => {
    const permission = permissionFor(params.threadId);
    if (permission === undefined) return { decision: 'cancel' };
    if (permission === 'fullAccess') return { decision: 'accept' };

    return new Promise<{ decision: ApprovalDecision }>((resolve) => {
      pendingCommands.set(requestId, {
        details: {
          kind: 'command',
          requestId,
          threadId: params.threadId,
          turnId: params.turnId,
          itemId: params.itemId,
          command: params.command ?? null,
          reason: params.reason ?? null,
          cwd: params.cwd ?? null,
        },
        resolve,
      });
    });
  });

  codex.onServerRequest('item/fileChange/requestApproval', (params, requestId) => {
    const permission = permissionFor(params.threadId);
    if (permission === undefined) return { decision: 'cancel' };
    if (permission === 'fullAccess') return { decision: 'accept' };

    return new Promise<{ decision: ApprovalDecision }>((resolve) => {
      const item = fileItems.get(itemKey(params.threadId, params.turnId, params.itemId));
      pendingFiles.set(requestId, {
        details: {
          kind: 'file',
          requestId,
          threadId: params.threadId,
          turnId: params.turnId,
          itemId: params.itemId,
          reason: params.reason ?? null,
          grantRoot: params.grantRoot ?? null,
          changes: item?.changes ?? null,
        },
        resolve,
      });
    });
  });

  function rememberFileItem({ threadId, turnId, item }: v2.ItemStartedNotification | v2.ItemCompletedNotification) {
    if (item.type !== 'fileChange') return;
    const changes = structuredClone(item.changes);
    fileItems.set(itemKey(threadId, turnId, item.id), { threadId, turnId, changes });
    for (const { details } of pendingFiles.values()) {
      if (details.threadId === threadId && details.turnId === turnId && details.itemId === item.id) {
        details.changes = changes;
      }
    }
  }

  codex.onNotification('item/started', rememberFileItem);
  codex.onNotification('item/completed', rememberFileItem);

  function clearOwned(threadId?: string, turnId?: string) {
    const matches = (owner: { threadId: string; turnId: string }) =>
      (threadId === undefined || owner.threadId === threadId) && (turnId === undefined || owner.turnId === turnId);
    for (const [requestId, { details }] of pendingCommands) {
      if (matches(details)) decide(pendingCommands, requestId, 'cancel');
    }
    for (const [requestId, { details }] of pendingFiles) {
      if (matches(details)) decide(pendingFiles, requestId, 'cancel');
    }
    for (const [key, item] of fileItems) {
      if (matches(item)) fileItems.delete(key);
    }
  }

  codex.onNotification('serverRequest/resolved', ({ threadId, requestId }) => {
    // Codex no longer needs an answer. The transport suppresses a late wire response.
    if (pendingCommands.get(requestId)?.details.threadId === threadId) decide(pendingCommands, requestId, 'cancel');
    if (pendingFiles.get(requestId)?.details.threadId === threadId) decide(pendingFiles, requestId, 'cancel');
  });
  codex.onNotification('turn/completed', ({ threadId, turn }) => clearOwned(threadId, turn.id));
  codex.onNotification('thread/closed', ({ threadId }) => clearOwned(threadId));
  codex.onExit(() => clearOwned());

  return {
    commandWaiting: (requestId: RequestId): PendingCommandApproval | undefined => {
      return structuredClone(pendingCommands.get(requestId)?.details);
    },
    fileWaiting: (requestId: RequestId): PendingFileApproval | undefined => {
      return structuredClone(pendingFiles.get(requestId)?.details);
    },
    approvalsWaiting: (threadId: string): Approval[] => {
      return structuredClone(
        [...pendingCommands.values(), ...pendingFiles.values()]
          .filter(({ details }) => details.threadId === threadId)
          .map(({ details }) => details),
      );
    },
    decideCommand: (requestId: RequestId, decision: ApprovalDecision) => decide(pendingCommands, requestId, decision),
    decideFile: (requestId: RequestId, decision: ApprovalDecision) => decide(pendingFiles, requestId, decision),
  };
}
