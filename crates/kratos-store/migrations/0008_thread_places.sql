-- Never edit this file: databases in the field have already run it.
-- Version 8 lets a thread work in a git worktree instead of its project folder. A thread with no
-- worktree works in its project folder. `creating` marks a worktree Kratos reserved before git
-- made it, so a restart can tell its own leftover from a folder the user made.
  CREATE TABLE worktrees (
    id TEXT PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    repository TEXT NOT NULL,
    branch TEXT,
    ownership TEXT NOT NULL CHECK (ownership IN ('createdByKratos', 'borrowed')),
    state TEXT NOT NULL CHECK (state IN ('creating', 'ready')),
    created_at TEXT NOT NULL,
    CHECK (ownership = 'createdByKratos' OR state = 'ready'),
    CHECK (ownership = 'borrowed' OR branch IS NOT NULL)
  );
  ALTER TABLE threads ADD COLUMN worktree_id TEXT REFERENCES worktrees (id);
  CREATE INDEX threads_by_worktree ON threads (worktree_id) WHERE worktree_id IS NOT NULL;
