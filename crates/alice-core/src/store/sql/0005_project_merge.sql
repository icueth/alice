-- Alice schema v5: merging one project into another (`alice projects merge`).
-- The merged project's stable key keeps resolving, to the project it was merged into.
CREATE TABLE project_key_aliases (
  key        TEXT PRIMARY KEY,
  project_id TEXT NOT NULL REFERENCES projects(id)
) WITHOUT ROWID;
-- The rows a merge moved (kind: memory | source | chunk), so that undo moves exactly those back.
-- Kept in a table of their own because a palace import can mean over a hundred thousand ids,
-- too many for an event payload; a row goes when its merge is undone.
CREATE TABLE project_merge_rows (
  merge_id TEXT NOT NULL,
  kind     TEXT NOT NULL,
  row_id   TEXT NOT NULL,
  PRIMARY KEY (merge_id, kind, row_id)
) WITHOUT ROWID;
