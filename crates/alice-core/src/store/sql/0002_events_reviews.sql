-- Alice schema v2: history and the review inbox (spec §5.4, §5.5).
-- A change that touches several rows (a supersede, a judge decision, a merge) shares one
-- group_id; undo reverts a whole group.
ALTER TABLE events ADD COLUMN group_id TEXT;
CREATE INDEX events_target ON events(target_type, target_id, ts);
CREATE INDEX events_ts ON events(ts);
CREATE INDEX events_group ON events(group_id) WHERE group_id IS NOT NULL;

-- One open review item per question: the judge or a weekly job asking again finds it.
ALTER TABLE review_items ADD COLUMN dedupe_key TEXT;
CREATE UNIQUE INDEX review_open_dedupe ON review_items(dedupe_key)
  WHERE dedupe_key IS NOT NULL AND status = 'open';
CREATE INDEX review_status ON review_items(status, created_at);
