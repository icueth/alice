-- Alice schema v8: a session keeps its best distilled memories (SP2 Task 12c, spec §6.4, §8.1).
-- The rank inputs of a distilled memory; NULL for every memory written before v8 (ranked as a
-- main-session passage at distill.min_confidence) and for every memory that is not distilled.
ALTER TABLE memories ADD COLUMN distill_p REAL;     -- the probability Jev gave its kind
ALTER TABLE memories ADD COLUMN distill_tier TEXT;  -- 'main' | 'subagent'
-- A session's distilled memories, read once per distill run (`store::capture::SESSION_HELD`).
CREATE INDEX memories_distill_session ON memories(author_session, author_agent)
  WHERE origin = 'distill';
-- When a person or an agent last changed this distilled memory (an event on it by `user` or
-- `agent:*`, set by `events::record`): the distill cap never replaces it (Task 12c addendum).
-- Kept on the row, so it outlives the history (`maintenance.events_days`). Backfilled here.
ALTER TABLE memories ADD COLUMN touched_at TEXT;
UPDATE memories SET touched_at = (
    SELECT max(e.ts) FROM events e
     WHERE e.target_type = 'memory' AND e.target_id = memories.id
       AND (e.actor = 'user' OR e.actor LIKE 'agent:%'))
 WHERE origin = 'distill';
-- The facts that cite a memory, read for each member of a session's best set.
CREATE INDEX relations_source_memory ON relations(source_memory_id)
  WHERE source_memory_id IS NOT NULL;
-- A session's captured turns, which its best set's room grows with (Task 12d).
CREATE INDEX capture_turns_source ON capture_turns(source_id);
