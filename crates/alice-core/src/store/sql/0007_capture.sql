-- Alice schema v7: automatic capture of agent transcripts (SP2 spec §8.1).
CREATE TABLE capture_state(
  source_id        TEXT PRIMARY KEY REFERENCES sources(id) ON DELETE CASCADE,
  agent            TEXT NOT NULL,
  session_id       TEXT NOT NULL,
  parent_source_id TEXT,                       -- a subagent's main session
  transcript_path  TEXT,                       -- local only; NULL in tenant mode
  file_key         TEXT,                       -- local only: dev:inode:sha256(first line)
  byte_offset      INTEGER NOT NULL DEFAULT 0,
  next_ord         INTEGER NOT NULL DEFAULT 0,
  last_event       TEXT,
  last_capture_at  TEXT,
  ended            INTEGER NOT NULL DEFAULT 0,
  no_distill       INTEGER NOT NULL DEFAULT 0, -- tenant client flag
  gaps             INTEGER NOT NULL DEFAULT 0, -- tenant forward gaps accepted
  distill_ord      INTEGER NOT NULL DEFAULT 0, -- chunks with ord < this were distilled
  distill_state    TEXT NOT NULL DEFAULT 'idle',
      -- idle|due|done|waiting_budget|waiting_judge|skipped_cloud|skipped_excluded|skipped_backfill|failed
  distill_attempts INTEGER NOT NULL DEFAULT 0,
  distill_next_try_at TEXT,
  created_at       TEXT NOT NULL
);
CREATE INDEX capture_state_distill ON capture_state(distill_state, distill_next_try_at);
CREATE INDEX capture_state_session ON capture_state(agent, session_id);
CREATE INDEX capture_state_parent ON capture_state(parent_source_id);

CREATE TABLE capture_turns(
  agent TEXT NOT NULL, turn_key TEXT NOT NULL, source_id TEXT NOT NULL, first_ord INTEGER NOT NULL,
  PRIMARY KEY(agent, turn_key)
) WITHOUT ROWID;                               -- kept when a source is deleted (§3.5)

ALTER TABLE memories ADD COLUMN origin TEXT NOT NULL DEFAULT 'agent';   -- agent | distill | import
UPDATE memories SET origin = 'import' WHERE judge_state = 'skipped';    -- M3 imports
CREATE INDEX sources_session ON sources(session_id) WHERE session_id IS NOT NULL;
