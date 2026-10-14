-- Alice schema v3: what the judge needs per memory (spec §5.1 step 5, §5.2).
ALTER TABLE memories ADD COLUMN kind_explicit INTEGER NOT NULL DEFAULT 1;      -- 0: kind defaulted to note
ALTER TABLE memories ADD COLUMN project_defaulted INTEGER NOT NULL DEFAULT 0;  -- 1: global only because no project was known
ALTER TABLE memories ADD COLUMN judge_attempts INTEGER NOT NULL DEFAULT 0;
CREATE INDEX memories_judge ON memories(judge_state) WHERE judge_state <> 'done';
-- Memories written before the judge existed are not judged after the fact. Only 'pending' rows
-- that are already live change; 'skipped' (imports) and 'retry' rows keep their state.
UPDATE memories SET judge_state = 'done' WHERE judge_state = 'pending' AND status <> 'pending';
