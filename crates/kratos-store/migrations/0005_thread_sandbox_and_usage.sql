-- Never edit this file: databases in the field have already run it.
  ALTER TABLE threads ADD COLUMN model TEXT;
  ALTER TABLE threads ADD COLUMN permission TEXT NOT NULL DEFAULT 'askMe' CHECK (permission IN ('askMe', 'approveForMe', 'fullAccess'));
  ALTER TABLE threads ADD COLUMN plan_mode INTEGER NOT NULL DEFAULT 0 CHECK (plan_mode IN (0, 1));
  ALTER TABLE threads ADD COLUMN context_limit_tokens INTEGER CHECK (context_limit_tokens > 0);
  ALTER TABLE threads ADD COLUMN sandbox TEXT;
  ALTER TABLE threads ADD COLUMN paused TEXT;
  ALTER TABLE threads ADD COLUMN running_turn TEXT;
  ALTER TABLE provider_sessions ADD COLUMN plan TEXT;
  CREATE INDEX provider_sessions_by_native_id ON provider_sessions (provider, native_id);
  CREATE TABLE thread_messages (
    thread_id TEXT NOT NULL REFERENCES threads (id),
    id TEXT NOT NULL,
    position INTEGER NOT NULL,
    input TEXT NOT NULL,
    origin TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('queued', 'submitting', 'accepted', 'failed', 'unknown', 'cancelled')),
    turn_id TEXT,
    error TEXT,
    queued_at TEXT NOT NULL,
    PRIMARY KEY (thread_id, id),
    UNIQUE (thread_id, position),
    CHECK (status != 'accepted' OR turn_id IS NOT NULL),
    CHECK ((status IN ('failed', 'unknown')) = (error IS NOT NULL))
  ) WITHOUT ROWID;
  CREATE TABLE approvals (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads (id),
    approval TEXT NOT NULL,
    decision TEXT CHECK (decision IN ('accept', 'acceptForSession', 'decline', 'cancel')),
    closed_reason TEXT,
    requested_at TEXT NOT NULL,
    settled_at TEXT,
    CHECK (decision IS NULL OR closed_reason IS NULL),
    CHECK ((settled_at IS NULL) = (decision IS NULL AND closed_reason IS NULL))
  );
  CREATE INDEX approvals_waiting ON approvals (thread_id) WHERE settled_at IS NULL;
  CREATE TABLE questions (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads (id),
    question TEXT NOT NULL,
    answered INTEGER NOT NULL DEFAULT 0 CHECK (answered IN (0, 1)),
    closed_reason TEXT,
    asked_at TEXT NOT NULL,
    settled_at TEXT,
    CHECK (answered = 0 OR closed_reason IS NULL),
    CHECK ((settled_at IS NULL) = (answered = 0 AND closed_reason IS NULL))
  );
  CREATE INDEX questions_waiting ON questions (thread_id) WHERE settled_at IS NULL;
  CREATE TABLE continuations (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL REFERENCES threads (id),
    turn_id TEXT NOT NULL,
    waits_on TEXT NOT NULL,
    resets_at TEXT,
    state TEXT NOT NULL CHECK (state IN ('scheduled', 'awaitingUser', 'sent', 'cancelled')),
    detail TEXT,
    message_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    CHECK ((state = 'sent') = (message_id IS NOT NULL))
  );
  CREATE UNIQUE INDEX open_continuation_per_thread ON continuations (thread_id) WHERE state IN ('scheduled', 'awaitingUser');
