import { randomUUID, type UUID } from 'node:crypto';
import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { z } from 'zod';
import type { v2 } from '@kratos/codex';
import {
  providers,
  threadEventBodySchema,
  type CreateThreadBody,
  type CreateWorkspaceBody,
  type Provider,
  type Thread,
  type ThreadEvent,
  type ThreadEventBody,
  type Workspace,
} from '@kratos/protocol';

const imageDetail = z.enum(['auto', 'low', 'high', 'original']).optional();
export const codexInputSchema = z
  .array(
    z.union([
      z.object({
        type: z.literal('text'),
        text: z.string(),
        text_elements: z.array(
          z.object({
            byteRange: z.object({ start: z.number().int().nonnegative(), end: z.number().int().nonnegative() }),
            placeholder: z.string().nullable(),
          }),
        ),
      }),
      z
        .object({ type: z.literal('image'), url: z.string(), detail: imageDetail })
        .transform(({ detail, ...image }) => (detail === undefined ? image : { ...image, detail })),
      z
        .object({ type: z.literal('image'), fileId: z.string(), detail: imageDetail })
        .transform(({ detail, ...image }) => (detail === undefined ? image : { ...image, detail })),
      z
        .object({ type: z.literal('localImage'), path: z.string(), detail: imageDetail })
        .transform(({ detail, ...image }) => (detail === undefined ? image : { ...image, detail })),
      z.object({ type: z.literal('audio'), url: z.string() }),
      z.object({ type: z.literal('localAudio'), path: z.string() }),
      z.object({ type: z.literal('skill'), name: z.string(), path: z.string() }),
      z.object({ type: z.literal('mention'), name: z.string(), path: z.string() }),
    ]),
  )
  .min(1);
const messageStatus = z.enum(['queued', 'submitting', 'accepted', 'failed', 'unknown', 'cancelled']);
const permission = z.enum(['askMe', 'approveForMe', 'fullAccess']);
export interface CodexMessage {
  id: string;
  threadId: string;
  input: v2.UserInput[];
  status: z.infer<typeof messageStatus>;
  turnId: string | null;
  error: string | null;
}
export interface CodexThread {
  id: string;
  folder: string;
  model: string;
  permissions: z.infer<typeof permission>;
  plan: boolean;
  pausedError: string | null;
}

export const claudeInputSchema = z
  .array(
    z.union([
      z.object({ type: z.literal('text'), text: z.string() }),
      z.object({
        type: z.literal('image'),
        source: z.union([
          z.object({
            type: z.literal('base64'),
            media_type: z.enum(['image/png', 'image/jpeg', 'image/gif', 'image/webp']),
            data: z.string(),
          }),
          z.object({ type: z.literal('url'), url: z.string() }),
        ]),
      }),
    ]),
  )
  .min(1);
export type ClaudeInput = z.infer<typeof claudeInputSchema>;
// The ID doubles as the SDKUserMessage uuid, which Claude Code requires to be a UUID.
const claudeMessageId = z.uuid().transform((id) => id as UUID);
export interface ClaudeMessage {
  id: UUID;
  threadId: string;
  input: ClaudeInput;
  status: z.infer<typeof messageStatus>;
  turnId: string | null;
  error: string | null;
}
export interface ClaudeThread {
  id: string;
  folder: string;
  model: string | null;
  permissions: z.infer<typeof permission>;
  plan: boolean;
  pausedError: string | null;
  runningTurn: string | null;
}

const providerList = providers.map((provider) => `'${provider}'`).join(', ');

// Append only. Each entry runs once, in order, and PRAGMA user_version records how many have run.
const migrations: readonly string[] = [
  `
  CREATE TABLE workspaces (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    path TEXT NOT NULL,
    created_at TEXT NOT NULL
  );
  CREATE TABLE threads (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces (id),
    title TEXT NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN (${providerList})),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
  );
  CREATE INDEX threads_by_workspace ON threads (workspace_id);
  CREATE TABLE provider_sessions (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads (id),
    provider TEXT NOT NULL CHECK (provider IN (${providerList})),
    native_id TEXT,
    started_at TEXT NOT NULL,
    ended_at TEXT
  );
  CREATE INDEX provider_sessions_by_thread ON provider_sessions (thread_id);
  CREATE TABLE thread_events (
    thread_id TEXT NOT NULL REFERENCES threads (id),
    seq INTEGER NOT NULL,
    at TEXT NOT NULL,
    event TEXT NOT NULL,
    PRIMARY KEY (thread_id, seq)
  ) WITHOUT ROWID;
  `,
  `
  CREATE TABLE codex_threads (
    id TEXT PRIMARY KEY,
    folder TEXT NOT NULL,
    model TEXT NOT NULL,
    permissions TEXT NOT NULL CHECK (permissions IN ('askMe', 'approveForMe', 'fullAccess')),
    paused_error TEXT
  );
  CREATE TABLE codex_messages (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES codex_threads (id),
    input TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('queued', 'submitting', 'accepted', 'failed', 'unknown', 'cancelled')),
    turn_id TEXT,
    error TEXT,
    CHECK (status != 'accepted' OR turn_id IS NOT NULL)
  );
  CREATE INDEX codex_messages_by_thread ON codex_messages (thread_id);
  `,
  `
  CREATE TABLE claude_threads (
    id TEXT PRIMARY KEY,
    folder TEXT NOT NULL,
    model TEXT,
    permissions TEXT NOT NULL CHECK (permissions IN ('askMe', 'approveForMe', 'fullAccess')),
    paused_error TEXT,
    running_turn TEXT
  );
  CREATE TABLE claude_messages (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES claude_threads (id),
    input TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('queued', 'submitting', 'accepted', 'failed', 'unknown', 'cancelled')),
    turn_id TEXT,
    error TEXT,
    CHECK (status != 'accepted' OR turn_id IS NOT NULL)
  );
  CREATE INDEX claude_messages_by_thread ON claude_messages (thread_id);
  `,
  `
  ALTER TABLE codex_threads ADD COLUMN plan INTEGER NOT NULL DEFAULT 0;
  ALTER TABLE claude_threads ADD COLUMN plan INTEGER NOT NULL DEFAULT 0;
  `,
];

interface WorkspaceRow {
  id: string;
  name: string;
  path: string;
  created_at: string;
}

interface ThreadRow {
  id: string;
  workspace_id: string;
  title: string;
  provider: Provider;
  created_at: string;
  updated_at: string;
}

interface EventRow {
  thread_id: string;
  seq: number;
  at: string;
  event: string;
}

function inTransaction<T>(db: DatabaseSync, work: () => T): T {
  db.exec('BEGIN IMMEDIATE');
  try {
    const result = work();
    db.exec('COMMIT');
    return result;
  } catch (error) {
    db.exec('ROLLBACK');
    throw error;
  }
}

function migrate(db: DatabaseSync): void {
  const row = db.prepare('PRAGMA user_version').get() as { user_version: number } | undefined;
  const applied = row?.user_version ?? 0;
  if (applied > migrations.length) {
    throw new Error(`Database schema version ${applied} is newer than this build supports (${migrations.length})`);
  }
  for (const [index, sql] of migrations.entries()) {
    if (index < applied) continue;
    inTransaction(db, () => {
      db.exec(sql);
      db.exec(`PRAGMA user_version = ${index + 1}`);
    });
  }
}

const toWorkspace = (row: WorkspaceRow): Workspace => ({
  id: row.id,
  name: row.name,
  path: row.path,
  createdAt: row.created_at,
});

const toThread = (row: ThreadRow): Thread => ({
  id: row.id,
  workspaceId: row.workspace_id,
  title: row.title,
  provider: row.provider,
  createdAt: row.created_at,
  updatedAt: row.updated_at,
});

const toThreadEvent = (row: EventRow): ThreadEvent => ({
  threadId: row.thread_id,
  seq: row.seq,
  at: row.at,
  event: threadEventBodySchema.parse(JSON.parse(row.event)),
});

export class Store {
  private readonly db: DatabaseSync;

  constructor(home: string) {
    mkdirSync(home, { recursive: true });
    this.db = new DatabaseSync(join(home, 'kratos.db'));
    this.db.exec('PRAGMA journal_mode = WAL');
    this.db.exec('PRAGMA foreign_keys = ON');
    migrate(this.db);
  }

  close(): void {
    this.db.close();
  }

  listWorkspaces(): Workspace[] {
    const rows = this.db.prepare('SELECT * FROM workspaces ORDER BY created_at, rowid').all();
    return (rows as unknown as WorkspaceRow[]).map(toWorkspace);
  }

  getWorkspace(id: string): Workspace | undefined {
    const row = this.db.prepare('SELECT * FROM workspaces WHERE id = ?').get(id) as unknown as WorkspaceRow | undefined;
    return row && toWorkspace(row);
  }

  createWorkspace({ name, path }: CreateWorkspaceBody): Workspace {
    const workspace: Workspace = { id: randomUUID(), name, path, createdAt: new Date().toISOString() };
    this.db
      .prepare('INSERT INTO workspaces (id, name, path, created_at) VALUES (?, ?, ?, ?)')
      .run(workspace.id, workspace.name, workspace.path, workspace.createdAt);
    return workspace;
  }

  listThreads(workspaceId: string): Thread[] {
    const rows = this.db
      .prepare('SELECT * FROM threads WHERE workspace_id = ? ORDER BY created_at, rowid')
      .all(workspaceId);
    return (rows as unknown as ThreadRow[]).map(toThread);
  }

  getThread(id: string): Thread | undefined {
    const row = this.db.prepare('SELECT * FROM threads WHERE id = ?').get(id) as unknown as ThreadRow | undefined;
    return row && toThread(row);
  }

  createThread({ workspaceId, title, provider }: CreateThreadBody): Thread {
    const now = new Date().toISOString();
    const thread: Thread = { id: randomUUID(), workspaceId, title, provider, createdAt: now, updatedAt: now };
    this.db
      .prepare(
        'INSERT INTO threads (id, workspace_id, title, provider, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)',
      )
      .run(thread.id, thread.workspaceId, thread.title, thread.provider, thread.createdAt, thread.updatedAt);
    return thread;
  }

  appendEvent(threadId: string, event: ThreadEventBody): ThreadEvent {
    const at = new Date().toISOString();
    return inTransaction(this.db, () => {
      const touched = this.db.prepare('UPDATE threads SET updated_at = ? WHERE id = ?').run(at, threadId);
      if (touched.changes === 0) throw new Error(`Unknown thread ${threadId}`);
      const next = this.db
        .prepare('SELECT COALESCE(MAX(seq), 0) + 1 AS seq FROM thread_events WHERE thread_id = ?')
        .get(threadId) as unknown as { seq: number };
      this.db
        .prepare('INSERT INTO thread_events (thread_id, seq, at, event) VALUES (?, ?, ?, ?)')
        .run(threadId, next.seq, at, JSON.stringify(event));
      return { threadId, seq: next.seq, at, event };
    });
  }

  eventsAfter(threadId: string, after: number): ThreadEvent[] {
    const rows = this.db
      .prepare('SELECT thread_id, seq, at, event FROM thread_events WHERE thread_id = ? AND seq > ? ORDER BY seq')
      .all(threadId, after);
    return (rows as unknown as EventRow[]).map(toThreadEvent);
  }

  saveCodexThread(thread: CodexThread): void {
    this.db
      .prepare(
        `INSERT INTO codex_threads (id, folder, model, permissions, plan, paused_error) VALUES (?, ?, ?, ?, ?, ?)
      ON CONFLICT (id) DO UPDATE SET folder = excluded.folder, model = excluded.model,
      permissions = excluded.permissions, plan = excluded.plan, paused_error = excluded.paused_error`,
      )
      .run(thread.id, thread.folder, thread.model, thread.permissions, thread.plan ? 1 : 0, thread.pausedError);
  }

  getCodexThread(id: string): CodexThread | undefined {
    const row = this.db.prepare('SELECT * FROM codex_threads WHERE id = ?').get(id);
    if (!row) return undefined;
    return z
      .object({
        id: z.string(),
        folder: z.string(),
        model: z.string(),
        permissions: permission,
        plan: z.boolean(),
        pausedError: z.string().nullable(),
      })
      .parse({ ...row, plan: row.plan === 1, pausedError: row.paused_error });
  }

  addCodexMessage(id: string, threadId: string, input: v2.UserInput[]): CodexMessage {
    const parsed = codexInputSchema.parse(input);
    const existing = this.getCodexMessage(id);
    if (existing) {
      if (existing.threadId !== threadId || JSON.stringify(existing.input) !== JSON.stringify(parsed)) {
        throw new Error('Message ID already belongs to a different submission');
      }
      return existing;
    }
    this.db
      .prepare("INSERT INTO codex_messages (id, thread_id, input, status) VALUES (?, ?, ?, 'queued')")
      .run(id, threadId, JSON.stringify(parsed));
    return { id, threadId, input: parsed, status: 'queued', turnId: null, error: null };
  }

  getCodexMessage(id: string): CodexMessage | undefined {
    const row = this.db.prepare('SELECT * FROM codex_messages WHERE id = ?').get(id);
    return row ? parseCodexMessage(row) : undefined;
  }

  codexMessages(threadId: string): CodexMessage[] {
    return this.db
      .prepare('SELECT * FROM codex_messages WHERE thread_id = ? ORDER BY rowid')
      .all(threadId)
      .map(parseCodexMessage);
  }

  updateCodexMessage(
    id: string,
    status: CodexMessage['status'],
    turnId: string | null = null,
    error: string | null = null,
  ): CodexMessage {
    this.db
      .prepare('UPDATE codex_messages SET status = ?, turn_id = ?, error = ? WHERE id = ?')
      .run(status, turnId, error, id);
    const message = this.getCodexMessage(id);
    if (!message) throw new Error(`Unknown message ${id}`);
    return message;
  }

  saveClaudeThread(thread: ClaudeThread): void {
    this.db
      .prepare(
        `INSERT INTO claude_threads (id, folder, model, permissions, plan, paused_error, running_turn)
      VALUES (?, ?, ?, ?, ?, ?, ?)
      ON CONFLICT (id) DO UPDATE SET folder = excluded.folder, model = excluded.model,
      permissions = excluded.permissions, plan = excluded.plan, paused_error = excluded.paused_error,
      running_turn = excluded.running_turn`,
      )
      .run(
        thread.id,
        thread.folder,
        thread.model,
        thread.permissions,
        thread.plan ? 1 : 0,
        thread.pausedError,
        thread.runningTurn,
      );
  }

  getClaudeThread(id: string): ClaudeThread | undefined {
    const row = this.db.prepare('SELECT * FROM claude_threads WHERE id = ?').get(id);
    if (!row) return undefined;
    return z
      .object({
        id: z.string(),
        folder: z.string(),
        model: z.string().nullable(),
        permissions: permission,
        plan: z.boolean(),
        pausedError: z.string().nullable(),
        runningTurn: z.string().nullable(),
      })
      .parse({ ...row, plan: row.plan === 1, pausedError: row.paused_error, runningTurn: row.running_turn });
  }

  addClaudeMessage(id: string, threadId: string, input: ClaudeInput): ClaudeMessage {
    const messageId = claudeMessageId.parse(id);
    const parsed = claudeInputSchema.parse(input);
    const existing = this.getClaudeMessage(messageId);
    if (existing) {
      if (existing.threadId !== threadId || JSON.stringify(existing.input) !== JSON.stringify(parsed)) {
        throw new Error('Message ID already belongs to a different submission');
      }
      return existing;
    }
    this.db
      .prepare("INSERT INTO claude_messages (id, thread_id, input, status) VALUES (?, ?, ?, 'queued')")
      .run(messageId, threadId, JSON.stringify(parsed));
    return { id: messageId, threadId, input: parsed, status: 'queued', turnId: null, error: null };
  }

  getClaudeMessage(id: string): ClaudeMessage | undefined {
    const row = this.db.prepare('SELECT * FROM claude_messages WHERE id = ?').get(id);
    return row ? parseClaudeMessage(row) : undefined;
  }

  claudeMessages(threadId: string): ClaudeMessage[] {
    return this.db
      .prepare('SELECT * FROM claude_messages WHERE thread_id = ? ORDER BY rowid')
      .all(threadId)
      .map(parseClaudeMessage);
  }

  updateClaudeMessage(
    id: string,
    status: ClaudeMessage['status'],
    turnId: string | null = null,
    error: string | null = null,
  ): ClaudeMessage {
    this.db
      .prepare('UPDATE claude_messages SET status = ?, turn_id = ?, error = ? WHERE id = ?')
      .run(status, turnId, error, id);
    const message = this.getClaudeMessage(id);
    if (!message) throw new Error(`Unknown message ${id}`);
    return message;
  }
}

function parseClaudeMessage(row: unknown): ClaudeMessage {
  const parsed = z
    .object({
      id: claudeMessageId,
      thread_id: z.string(),
      input: z.string(),
      status: messageStatus,
      turn_id: z.string().nullable(),
      error: z.string().nullable(),
    })
    .parse(row);
  return {
    id: parsed.id,
    threadId: parsed.thread_id,
    input: claudeInputSchema.parse(JSON.parse(parsed.input)),
    status: parsed.status,
    turnId: parsed.turn_id,
    error: parsed.error,
  };
}

function parseCodexMessage(row: unknown): CodexMessage {
  const parsed = z
    .object({
      id: z.string(),
      thread_id: z.string(),
      input: z.string(),
      status: messageStatus,
      turn_id: z.string().nullable(),
      error: z.string().nullable(),
    })
    .parse(row);
  return {
    id: parsed.id,
    threadId: parsed.thread_id,
    input: codexInputSchema.parse(JSON.parse(parsed.input)),
    status: parsed.status,
    turnId: parsed.turn_id,
    error: parsed.error,
  };
}
