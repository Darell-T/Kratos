-- Never edit this file: databases in the field have already run it.
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
    provider TEXT NOT NULL CHECK (provider IN ('codex', 'claude')),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
  );
  CREATE INDEX threads_by_workspace ON threads (workspace_id);
  CREATE TABLE provider_sessions (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads (id),
    provider TEXT NOT NULL CHECK (provider IN ('codex', 'claude')),
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
