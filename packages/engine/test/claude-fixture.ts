import { randomUUID } from 'node:crypto';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { afterEach, vi } from 'vitest';
import type {
  AccountInfo,
  CanUseTool,
  ElicitationRequest,
  ModelInfo,
  OnElicitation,
  Options,
  PermissionUpdate,
  SDKMessage,
  SDKUserMessage,
  SlashCommand,
} from '@anthropic-ai/claude-agent-sdk';
import type { ClaudeEvent, ClaudeProvider, ClaudeProviderOptions } from '../src/providers/claude';
import { Store, type ClaudeInput } from '../src/store';

export const A = '00000000-0000-4000-8000-00000000000a';
export const B = '00000000-0000-4000-8000-00000000000b';
export const C = '00000000-0000-4000-8000-00000000000c';
export const D = '00000000-0000-4000-8000-00000000000d';
export const E = '00000000-0000-4000-8000-00000000000e';
export const folder = 'C:\\project-A';

export interface Launch {
  options: Options;
  written: SDKUserMessage[];
  interrupts: number;
  hungInterrupts: ((error: Error) => void)[];
  closed: boolean;
  emit(message: SDKMessage): void;
  exit(error?: Error): void;
}

const flushEvents = globalThis.setImmediate;
const contexts: { provider: ClaudeProvider; store: Store; home: string }[] = [];

afterEach(async () => {
  vi.useRealTimers();
  for (const { provider, store, home } of contexts.splice(0)) {
    await provider.close();
    store.close();
    if (dirname(home) !== tmpdir()) throw new Error('Invalid test directory');
    rmSync(home, { recursive: true, force: true });
  }
  vi.doUnmock('@anthropic-ai/claude-agent-sdk');
  vi.restoreAllMocks();
});

export function input(text: string): ClaudeInput {
  return [{ type: 'text', text }];
}

export const flush = () => new Promise<void>((resolve) => flushEvents(resolve));

export interface FakeCli {
  account: AccountInfo;
  models: ModelInfo[];
  commands: SlashCommand[];
  plugins: { name: string; path: string; source?: string }[];
}

function fakeQuery(
  prompt: AsyncIterable<SDKUserMessage>,
  options: Options,
  interruptOutcome: () => Error | 'hang' | undefined,
  cli: FakeCli,
) {
  const output: SDKMessage[] = [];
  let wake: (() => void) | undefined;
  let ended: { error?: Error } | undefined;
  const launch: Launch = {
    options,
    written: [],
    interrupts: 0,
    hungInterrupts: [],
    closed: false,
    emit(message) {
      output.push(message);
      wake?.();
    },
    exit(error) {
      ended = error ? { error } : {};
      wake?.();
    },
  };
  void (async () => {
    for await (const message of prompt) launch.written.push(message);
  })();
  async function* messages(): AsyncGenerator<SDKMessage, void> {
    for (;;) {
      const next = output.shift();
      if (next) yield next;
      else if (ended?.error) throw ended.error;
      else if (ended) return;
      else await new Promise<void>((resolve) => (wake = resolve));
    }
  }
  const query = Object.assign(messages(), {
    interrupt() {
      launch.interrupts += 1;
      const outcome = interruptOutcome();
      if (outcome === 'hang') return new Promise<void>((_, reject) => launch.hungInterrupts.push(reject));
      return outcome ? Promise.reject(outcome) : Promise.resolve(undefined);
    },
    close() {
      launch.closed = true;
      for (const reject of launch.hungInterrupts) reject(new Error('Query closed before response received'));
      launch.exit();
    },
    initializationResult: () => Promise.resolve({ account: cli.account, models: cli.models, commands: cli.commands }),
    reloadPlugins: () => Promise.resolve({ plugins: cli.plugins }),
  });
  return { launch, query };
}

function fakeSdk() {
  const launches: Launch[] = [];
  const transcripts = new Map<string, string[]>();
  let interruptOutcome: Error | 'hang' | undefined;
  let transcriptRead = Promise.resolve();
  const cli: FakeCli = {
    account: { tokenSource: 'none', apiProvider: 'firstParty' },
    models: [],
    commands: [],
    plugins: [],
  };
  const transcriptKey = (sessionId: string, dir?: string) => JSON.stringify([dir, sessionId]);
  const module = {
    query: ({ prompt, options }: { prompt: AsyncIterable<SDKUserMessage>; options: Options }) => {
      const { launch, query } = fakeQuery(prompt, options, () => interruptOutcome, cli);
      launches.push(launch);
      return query;
    },
    getSessionInfo: (sessionId: string, { dir }: { dir?: string } = {}) =>
      Promise.resolve(transcripts.has(transcriptKey(sessionId, dir)) ? { sessionId } : undefined),
    getSessionMessages: (sessionId: string, { dir }: { dir?: string } = {}) =>
      transcriptRead.then(() =>
        (transcripts.get(transcriptKey(sessionId, dir)) ?? []).map((uuid) => ({
          type: 'user',
          uuid,
          session_id: sessionId,
          message: {},
          parent_tool_use_id: null,
          parent_agent_id: null,
        })),
      ),
  };
  const record = (sessionId: string, messageId: string) => {
    const key = transcriptKey(sessionId, folder);
    transcripts.set(key, [...(transcripts.get(key) ?? []), messageId]);
  };
  const failInterrupts = (outcome?: Error | 'hang') => {
    interruptOutcome = outcome;
  };
  const holdTranscripts = () => {
    let release = () => {};
    transcriptRead = new Promise((resolve) => (release = resolve));
    return release;
  };
  return { launches, module, record, failInterrupts, holdTranscripts, cli };
}

async function loadProvider(store: Store, module: object, options: Partial<Omit<ClaudeProviderOptions, 'store'>>) {
  vi.resetModules();
  vi.doMock('@anthropic-ai/claude-agent-sdk', () => module);
  const { createClaudeProvider } = await import('../src/providers/claude');
  return createClaudeProvider({ store, ...options });
}

export async function setup(options: Partial<Omit<ClaudeProviderOptions, 'store'>> = {}) {
  const home = mkdtempSync(join(tmpdir(), 'kratos-claude-'));
  const store = new Store(home);
  const { launches, module, record, failInterrupts, holdTranscripts, cli } = fakeSdk();
  const provider = await loadProvider(store, module, options);
  const context = { provider, store, home };
  contexts.push(context);
  const thread = await provider.startThread(folder);
  const events: ClaudeEvent[] = [];
  provider.onEvent((event) => {
    events.push(event);
  });
  const launch = () => {
    const current = launches.at(-1);
    if (!current) throw new Error('Claude was not launched');
    return current;
  };
  const emit = async (message: object) => {
    launch().emit({ session_id: thread, uuid: randomUUID(), ...message } as SDKMessage);
    await flush();
  };
  const accept = async (messageId: string) => {
    record(thread, messageId);
    await emit({
      type: 'user',
      message: { role: 'user', content: [] },
      parent_tool_use_id: null,
      uuid: messageId,
      isReplay: true,
    });
  };
  const init = (model = 'claude-test') => emit({ type: 'system', subtype: 'init', model });
  return {
    ...provider,
    provider,
    store,
    thread,
    events,
    launches,
    launch,
    record: (messageId: string) => record(thread, messageId),
    failInterrupts,
    holdTranscripts,
    cli,
    init,
    accept,
    start: async (messageId: string) => {
      const sent = provider.sendMessage(thread, input(messageId), messageId);
      await flush();
      await init();
      await accept(messageId);
      return sent;
    },
    written: () => launch().written.map((message) => message.uuid),
    finish: (messageIds: string[], outcome: object = {}) =>
      emit({
        type: 'result',
        subtype: 'success',
        is_error: false,
        result: 'Done',
        user_message_uuids: messageIds,
        ...outcome,
      }),
    output: (message: object) => emit(message),
    exit: async (error?: Error) => {
      launch().exit(error);
      await flush();
    },
    ask: (toolName: string, toolInput: Record<string, unknown>, details: Partial<Parameters<CanUseTool>[2]> = {}) => {
      const controller = new AbortController();
      const { canUseTool } = launch().options;
      if (!canUseTool) throw new Error('Claude was launched without canUseTool');
      const answer = canUseTool(toolName, toolInput, {
        signal: controller.signal,
        toolUseID: randomUUID(),
        requestId: randomUUID(),
        ...details,
      });
      return { answer, abort: () => controller.abort() };
    },
    elicit: (request: ElicitationRequest) => {
      const controller = new AbortController();
      const onElicitation: OnElicitation | undefined = launch().options.onElicitation;
      if (!onElicitation) throw new Error('Claude was launched without onElicitation');
      return {
        answer: onElicitation(request, { signal: controller.signal, requestId: randomUUID() }),
        abort: () => controller.abort(),
      };
    },
    restart: async () => {
      await context.provider.close();
      context.provider = await loadProvider(store, module, options);
      return context.provider;
    },
  };
}

export const sessionRule: PermissionUpdate = {
  type: 'addRules',
  rules: [{ toolName: 'Bash', ruleContent: 'npm test' }],
  behavior: 'allow',
  destination: 'session',
};
