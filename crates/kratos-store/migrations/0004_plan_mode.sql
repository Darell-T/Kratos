-- Never edit this file: databases in the field have already run it.
  ALTER TABLE codex_threads ADD COLUMN plan INTEGER NOT NULL DEFAULT 0;
  ALTER TABLE claude_threads ADD COLUMN plan INTEGER NOT NULL DEFAULT 0;
