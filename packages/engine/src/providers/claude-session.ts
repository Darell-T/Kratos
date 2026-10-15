import { z } from 'zod';
import {
  getSessionInfo,
  getSessionMessages,
  query,
  type CanUseTool,
  type HookCallback,
  type OnElicitation,
  type Options,
  type SDKMessage,
  type SDKResultMessage,
  type SDKUserMessage,
  type SettingSource,
} from '@anthropic-ai/claude-agent-sdk';
import type { ClaudeMessage } from '../store';
import type { Permission } from './claude-approvals';

export interface ClaudeSessionOptions {
  threadId: string;
  folder: string;
  model: string | null;
  permissions: Permission;
  readOnly: boolean;
  canUseTool: CanUseTool;
  onElicitation: OnElicitation;
  onToolUse: (use: { kind: 'edit'; path: string } | { kind: 'command' }) => void;
}
export type ClaudeSessionEvent =
  | { kind: 'initialized'; model: string }
  | { kind: 'accepted'; messageId: string }
  | { kind: 'output'; message: SDKMessage }
  | { kind: 'result'; messageIds: string[]; error: string | null; result: SDKResultMessage }
  | { kind: 'exited'; error: Error };
export interface ClaudeSession {
  submit(message: ClaudeMessage): void;
  interrupt(): Promise<void>;
  close(): void;
  readonly initialized: boolean;
}

const settingSources: SettingSource[] = ['user', 'project', 'local'];
const permissionModes = {
  askMe: { permissionMode: 'default' },
  approveForMe: { permissionMode: 'auto' },
  fullAccess: { permissionMode: 'bypassPermissions', allowDangerouslySkipPermissions: true },
} satisfies Record<Permission, Options>;
const readOnlyMode = { permissionMode: 'plan' } satisfies Options;

const editPathKeys: Record<string, 'file_path' | 'notebook_path'> = {
  Edit: 'file_path',
  Write: 'file_path',
  MultiEdit: 'file_path',
  NotebookEdit: 'notebook_path',
};
const commandTools = ['Bash', 'PowerShell'];
const toolUseSchema = z.object({
  tool_name: z.string(),
  tool_input: z.object({ file_path: z.string().optional(), notebook_path: z.string().optional() }),
});
const initSchema = z.object({ model: z.string().min(1) });
const replaySchema = z.object({ uuid: z.string().min(1) });
const resultSchema = z.object({
  subtype: z.string(),
  is_error: z.boolean(),
  result: z.string().optional(),
  errors: z.array(z.string()).optional(),
  user_message_uuid: z.string().optional(),
  user_message_uuids: z.array(z.string()).optional(),
});

function toolHooks(onToolUse: ClaudeSessionOptions['onToolUse']): NonNullable<Options['hooks']> {
  const hook =
    (succeeded: boolean): HookCallback =>
    (input) => {
      const { tool_name, tool_input } = toolUseSchema.parse(input);
      const pathKey = editPathKeys[tool_name];
      const path = pathKey && tool_input[pathKey];
      if (commandTools.includes(tool_name)) onToolUse({ kind: 'command' });
      else if (succeeded && path) onToolUse({ kind: 'edit', path });
      return Promise.resolve({ continue: true });
    };
  return { PostToolUse: [{ hooks: [hook(true)] }], PostToolUseFailure: [{ hooks: [hook(false)] }] };
}

function resultEvent(message: SDKResultMessage): ClaudeSessionEvent {
  const result = resultSchema.parse(message);
  const messageIds = result.user_message_uuids ?? (result.user_message_uuid ? [result.user_message_uuid] : []);
  const failure = result.subtype === 'success' ? result.result : result.errors?.join('\n');
  const failed = result.subtype !== 'success' || result.is_error;
  return { kind: 'result', messageIds, error: failed ? failure || 'Claude turn failed' : null, result: message };
}

function sessionEvent(message: SDKMessage): ClaudeSessionEvent {
  if (message.type === 'system' && message.subtype === 'init')
    return { kind: 'initialized', model: initSchema.parse(message).model };
  if (message.type === 'user' && 'isReplay' in message && message.isReplay)
    return { kind: 'accepted', messageId: replaySchema.parse(message).uuid };
  if (message.type === 'result') return resultEvent(message);
  return { kind: 'output', message };
}

function inputFeed() {
  const queued: SDKUserMessage[] = [];
  let wake: (() => void) | undefined;
  let ended = false;
  async function* feed(): AsyncGenerator<SDKUserMessage> {
    for (;;) {
      const next = queued.shift();
      if (next) yield next;
      else if (ended) return;
      else await new Promise<void>((resolve) => (wake = resolve));
    }
  }
  return {
    feed: feed(),
    push(message: SDKUserMessage) {
      queued.push(message);
      wake?.();
    },
    end() {
      ended = true;
      wake?.();
    },
  };
}

export async function openClaudeSession(
  options: ClaudeSessionOptions,
  onEvent: (event: ClaudeSessionEvent) => void,
): Promise<ClaudeSession> {
  const resume = (await getSessionInfo(options.threadId, { dir: options.folder })) !== undefined;
  const input = inputFeed();
  const claude = query({
    prompt: input.feed,
    options: {
      cwd: options.folder,
      ...(options.model ? { model: options.model } : {}),
      ...(options.readOnly ? readOnlyMode : permissionModes[options.permissions]),
      ...(resume ? { resume: options.threadId } : { sessionId: options.threadId }),
      settingSources,
      persistSession: true,
      includePartialMessages: true,
      extraArgs: { 'replay-user-messages': null },
      canUseTool: options.canUseTool,
      onElicitation: options.onElicitation,
      hooks: toolHooks(options.onToolUse),
    },
  });
  let initialized = false;
  void (async () => {
    let error = new Error('Claude Code exited');
    try {
      for await (const message of claude) {
        const event = sessionEvent(message);
        if (event.kind === 'initialized') initialized = true;
        onEvent(event);
      }
    } catch (reason) {
      error = reason instanceof Error ? reason : new Error(String(reason));
    }
    input.end();
    claude.close();
    onEvent({ kind: 'exited', error });
  })();
  return {
    submit(message) {
      input.push({
        type: 'user',
        message: { role: 'user', content: message.input },
        parent_tool_use_id: null,
        uuid: message.id,
      });
    },
    async interrupt() {
      await claude.interrupt();
    },
    close() {
      input.end();
      claude.close();
    },
    get initialized() {
      return initialized;
    },
  };
}

export async function deliveredMessageIds(threadId: string, folder: string): Promise<Set<string>> {
  const messages = await getSessionMessages(threadId, { dir: folder });
  return new Set(messages.filter((message) => message.type === 'user').map((message) => message.uuid));
}

export async function describeClaude(folder: string) {
  const input = inputFeed();
  const claude = query({ prompt: input.feed, options: { cwd: folder, settingSources } });
  try {
    const { models, commands, account } = await claude.initializationResult();
    const { plugins } = await claude.reloadPlugins();
    return { models, commands, account, plugins: plugins.map(({ name, path }) => ({ name, path })) };
  } finally {
    input.end();
    claude.close();
  }
}
