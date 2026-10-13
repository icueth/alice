-- Alice schema v1. Timestamps are RFC 3339 UTC strings with milliseconds.

CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE projects (
  id         TEXT PRIMARY KEY,
  key        TEXT NOT NULL UNIQUE,   -- normalized git remote, "path:<root>", or "global"
  slug       TEXT NOT NULL UNIQUE,
  name       TEXT NOT NULL,
  git_remote TEXT,
  root_path  TEXT,
  created_at TEXT NOT NULL
);

CREATE TABLE project_aliases (
  alias      TEXT PRIMARY KEY,       -- slugified
  project_id TEXT NOT NULL REFERENCES projects(id)
);

CREATE TABLE memories (
  seq             INTEGER PRIMARY KEY AUTOINCREMENT,   -- rowid; FTS and vector keys use it, so never reused
  id              TEXT NOT NULL UNIQUE,
  project_id      TEXT NOT NULL REFERENCES projects(id),
  kind            TEXT NOT NULL,
  title           TEXT NOT NULL,
  body            TEXT NOT NULL,
  importance      INTEGER NOT NULL DEFAULT 3 CHECK (importance BETWEEN 1 AND 5),
  status          TEXT NOT NULL,
  content_hash    TEXT NOT NULL,
  author_agent    TEXT,
  author_model    TEXT,
  author_session  TEXT,
  idempotency_key TEXT,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL,
  hlc             TEXT NOT NULL,
  access_count    INTEGER NOT NULL DEFAULT 0,
  last_accessed_at TEXT,
  legacy_id       TEXT,
  embed_state     TEXT NOT NULL DEFAULT 'pending',  -- pending | done | retry | failed
  judge_state     TEXT NOT NULL DEFAULT 'pending'   -- pending | done | retry | skipped
);
CREATE INDEX memories_project_kind ON memories(project_id, kind, status, created_at);
CREATE INDEX memories_project_importance ON memories(project_id, importance, updated_at) WHERE importance >= 4;
CREATE INDEX memories_project_kind_importance ON memories(project_id, kind, importance, updated_at);
CREATE UNIQUE INDEX memories_hash ON memories(project_id, content_hash) WHERE status <> 'trashed';
CREATE UNIQUE INDEX memories_idem ON memories(idempotency_key) WHERE idempotency_key IS NOT NULL;
CREATE INDEX memories_embed ON memories(embed_state) WHERE embed_state <> 'done';
CREATE INDEX memories_legacy ON memories(legacy_id) WHERE legacy_id IS NOT NULL;

CREATE TABLE memory_topics (
  memory_id TEXT NOT NULL,
  topic     TEXT NOT NULL,
  PRIMARY KEY (memory_id, topic)
);
CREATE INDEX memory_topics_topic ON memory_topics(topic);

CREATE TABLE sources (
  id          TEXT PRIMARY KEY,
  project_id  TEXT NOT NULL REFERENCES projects(id),
  type        TEXT NOT NULL,           -- session | file | import
  agent       TEXT,
  uri         TEXT NOT NULL,
  session_id  TEXT,
  started_at  TEXT,
  ended_at    TEXT,
  legacy_ref  TEXT,
  created_at  TEXT NOT NULL,
  UNIQUE (type, uri)
);

CREATE TABLE chunks (
  seq          INTEGER PRIMARY KEY AUTOINCREMENT,
  id           TEXT NOT NULL UNIQUE,
  source_id    TEXT NOT NULL REFERENCES sources(id),
  project_id   TEXT NOT NULL REFERENCES projects(id),
  ord          INTEGER NOT NULL,
  text         TEXT NOT NULL,
  content_hash TEXT NOT NULL,
  authored_at  TEXT,
  created_at   TEXT NOT NULL,
  embed_state  TEXT NOT NULL DEFAULT 'pending',
  legacy_id    TEXT,
  UNIQUE (source_id, ord)
);
CREATE INDEX chunks_embed ON chunks(embed_state) WHERE embed_state <> 'done';
CREATE INDEX chunks_project ON chunks(project_id, created_at);

CREATE TABLE provenance (
  memory_id TEXT NOT NULL,
  chunk_id  TEXT NOT NULL,
  PRIMARY KEY (memory_id, chunk_id)
);

CREATE TABLE links (
  from_id    TEXT NOT NULL,
  to_id      TEXT NOT NULL,
  type       TEXT NOT NULL,   -- relates_to | supersedes | contradicts | derived_from | duplicate_of
  created_by TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (from_id, to_id, type)
);
CREATE INDEX links_to ON links(to_id);

CREATE TABLE entities (
  id         TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  type       TEXT,
  created_at TEXT NOT NULL
);
CREATE TABLE entity_aliases (
  alias     TEXT PRIMARY KEY,   -- lowercase NFC, whitespace-collapsed
  entity_id TEXT NOT NULL REFERENCES entities(id)
);
CREATE TABLE predicates (
  name        TEXT PRIMARY KEY,
  description TEXT,
  canonical   TEXT              -- NULL when this name is canonical
);
CREATE TABLE relations (
  id               TEXT PRIMARY KEY,
  subject_id       TEXT NOT NULL REFERENCES entities(id),
  predicate        TEXT NOT NULL,
  object_entity_id TEXT REFERENCES entities(id),
  object_literal   TEXT,
  valid_from       TEXT NOT NULL,
  valid_to         TEXT,
  source_memory_id TEXT,
  created_at       TEXT NOT NULL,
  hlc              TEXT NOT NULL,
  CHECK ((object_entity_id IS NULL) <> (object_literal IS NULL))
);
CREATE INDEX relations_subject ON relations(subject_id, predicate);
CREATE INDEX relations_object ON relations(object_entity_id);
CREATE TABLE mentions (
  memory_id TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  PRIMARY KEY (memory_id, entity_id)
);
CREATE INDEX mentions_entity ON mentions(entity_id);

CREATE TABLE embeddings (
  owner_type TEXT NOT NULL,     -- memory | chunk
  owner_seq  INTEGER NOT NULL,
  model      TEXT NOT NULL,
  dim        INTEGER NOT NULL,
  vector     BLOB NOT NULL,     -- little-endian f16
  PRIMARY KEY (owner_type, owner_seq, model)
);

CREATE TABLE events (
  id          TEXT PRIMARY KEY,
  ts          TEXT NOT NULL,
  hlc         TEXT NOT NULL,
  actor       TEXT NOT NULL,
  action      TEXT NOT NULL,
  target_type TEXT NOT NULL,
  target_id   TEXT NOT NULL,
  before_json TEXT,
  after_json  TEXT
);
CREATE TABLE review_items (
  id           TEXT PRIMARY KEY,
  type         TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  confidence   REAL,
  status       TEXT NOT NULL,
  created_at   TEXT NOT NULL,
  resolved_at  TEXT,
  resolved_by  TEXT,
  resolution   TEXT
);
CREATE TABLE judge_cache (
  request_hash  TEXT PRIMARY KEY,
  provider      TEXT NOT NULL,
  model         TEXT NOT NULL,
  response_json TEXT NOT NULL,
  created_at    TEXT NOT NULL
);
CREATE TABLE cost_ledger (
  day           TEXT NOT NULL,
  provider      TEXT NOT NULL,
  calls         INTEGER NOT NULL DEFAULT 0,
  input_tokens  INTEGER NOT NULL DEFAULT 0,
  output_tokens INTEGER NOT NULL DEFAULT 0,
  usd           REAL NOT NULL DEFAULT 0,
  PRIMARY KEY (day, provider)
);
CREATE TABLE sync_state (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- Full-text indexes. Contentless: rows are maintained by store::fts, rowid = memories.seq / chunks.seq.
CREATE VIRTUAL TABLE memories_fts USING fts5(
  title, body, topics, code_tokens,
  content = '', contentless_delete = 1,
  tokenize = 'unicode61 remove_diacritics 2'
);
CREATE VIRTUAL TABLE memories_tri USING fts5(
  title, body,
  content = '', contentless_delete = 1,
  tokenize = 'trigram'
);
CREATE VIRTUAL TABLE chunks_fts USING fts5(
  text, code_tokens,
  content = '', contentless_delete = 1,
  tokenize = 'unicode61 remove_diacritics 2'
);
CREATE VIRTUAL TABLE chunks_tri USING fts5(
  text,
  content = '', contentless_delete = 1,
  tokenize = 'trigram'
);

INSERT INTO projects (id, key, slug, name, created_at)
VALUES ('00000000000000000000GLOBAL', 'global', 'global', 'Global', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
