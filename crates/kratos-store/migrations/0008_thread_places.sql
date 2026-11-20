-- Never edit this file: databases in the field have already run it.
-- Version 8 lets a thread work in a git worktree instead of its project folder, and lets the user
-- archive a thread. A thread with no worktree works in its project folder. `creating` marks a
-- worktree Kratos reserved before git made it, so a restart can tell its own leftover from a
-- folder the user made. `removing` marks one Kratos began to remove, which no thread may join.
-- A removed worktree's row stays for the threads that worked there, and frees its path. `base`
-- is the commit Kratos made a worktree from, which a thread's diff starts at.
  CREATE TABLE worktrees (
    id TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    repository TEXT NOT NULL,
    branch TEXT,
    base TEXT,
    ownership TEXT NOT NULL CHECK (ownership IN ('createdByKratos', 'borrowed')),
    state TEXT NOT NULL CHECK (state IN ('creating', 'ready', 'removing', 'removed')),
    created_at TEXT NOT NULL,
    CHECK (ownership = 'createdByKratos' OR state = 'ready'),
    CHECK (ownership = 'borrowed' OR (branch IS NOT NULL AND base IS NOT NULL))
  );
  CREATE UNIQUE INDEX worktrees_by_path ON worktrees (path) WHERE state != 'removed';
  ALTER TABLE threads ADD COLUMN worktree_id TEXT REFERENCES worktrees (id);
  ALTER TABLE threads ADD COLUMN archived_at TEXT;
  CREATE INDEX threads_by_worktree ON threads (worktree_id) WHERE worktree_id IS NOT NULL;
