-- Alice schema v8: a session keeps its best distilled memories (SP2 Task 12c, spec §6.4, §8.1).
-- The rank inputs of a distilled memory; NULL for every memory written before v8 (ranked as a
-- main-session passage at distill.min_confidence) and for every memory that is not distilled.
ALTER TABLE memories ADD COLUMN distill_p REAL;     -- the probability Jev gave its kind
ALTER TABLE memories ADD COLUMN distill_tier TEXT;  -- 'main' | 'subagent'
-- A session's distilled memories, read once per distill run (`store::capture::SESSION_HELD`).
CREATE INDEX memories_distill_session ON memories(author_session, author_agent)
  WHERE origin = 'distill';
