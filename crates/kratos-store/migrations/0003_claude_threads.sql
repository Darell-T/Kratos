-- Never edit this file: databases in the field have already run it.
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
