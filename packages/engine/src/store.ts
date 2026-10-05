import { randomUUID } from 'node:crypto';
import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
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
}
