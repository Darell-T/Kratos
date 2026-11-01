-- Never edit this file: databases in the field have already run it.
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
