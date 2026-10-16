-- Alice schema v6: the privacy rules a project merge (`alice projects merge`) carried into the
-- project that survived it: one row per merge, holding the merged project's rules at the time
-- (its `.alice.toml` and what earlier merges had carried into it). A project's carried rules are
-- all of its rows together, so undoing one merge takes back exactly what that merge carried and
-- nothing a later merge brought. The rows of a project that was merged away stay, unused, for
-- the undo that brings it back.
CREATE TABLE project_rules (
  project_id TEXT NOT NULL,
  merge_id   TEXT NOT NULL,
  rules_json TEXT NOT NULL,
  PRIMARY KEY (project_id, merge_id)
) WITHOUT ROWID;
CREATE INDEX project_rules_by_merge ON project_rules(merge_id);
