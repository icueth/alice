-- Alice schema v4: where each imported legacy id (a MemPalace drawer id, a KG triple id) went.
-- Collapsed duplicates point at the survivor, so compat lookups and incremental imports work.
CREATE TABLE legacy_ids (
  legacy_id   TEXT PRIMARY KEY,
  target_type TEXT NOT NULL,        -- memory | chunk | relation
  target_id   TEXT NOT NULL
) WITHOUT ROWID;
