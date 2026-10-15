# Alice Core — Design Spec (Sub-project 1)

- **Date:** 2026-10-13
- **Status:** M1 and M2 are built (alpha). Passages marked *As built* say what M2 decided or measured; everything from M3 on is still design.
- **Owner:** icueth
- **Scope of this document:** product framing for all of Alice, plus the full design of Sub-project 1 (Core). Sub-projects 2 and 3 get their own specs; this one only fixes the contracts Core must expose to them.

---

## 0. Context and intent

### 0.1 Why Alice exists

The owner runs many AI coding agents in parallel (Claude Code, Codex, OpenCode and others, often 10+ panes at once) and uses MemPalace 3.10 as their shared long-term memory. It works and they like it, but an audit of their palace on 2026-10-13 and their session history found recurring problems:

| # | Problem | Evidence |
|---|---|---|
| 1 | Writers block each other. The hub holds a single-writer lease while mining a transcript, so diary writes from other sessions time out after 120 s. | Repeated `palace hub proxy failed: timed out` (2026-09-30); 9 concurrent `mempalace mine` processes from per-session hooks (2026-10-11). |
| 2 | Local fixes are lost on upgrade. | Two patch files kept in `~/.mempalace/patches/`, re-applied by hand. |
| 3 | Two installations drift apart. | CLI from `~/.venv` (Python 3.13), hub and patches in a uv tool env (Python 3.11). |
| 4 | Wing sprawl. | 294 wings; 212 hold ≤ 3 drawers because every subagent's diary gets its own wing. Aliases split one project into `pg-ai` / `pg_ai` / `wing_pg_ai`. |
| 5 | Duplicates. | 5,396 exact-duplicate drawers. |
| 6 | Raw transcripts drown distilled knowledge. | `sessions` wing = 74.5k of 117k drawers; room `technical` = 66k. A search for "JEV คืออะไร" returned Thai fragments and hex blobs. |
| 7 | Knowledge graph is underused and inconsistent. | 153 triples vs 117k drawers; predicates `has_bug` / `had_bug` / `had_critical_bug`. |
| 8 | Wake-up costs tokens. | `status` returns all 294 wings every session. |
| 9 | Search slows linearly with data. | Brute-force vector scan; 909 MB DB + 91 MB WAL; 2.35 s CLI search at 117k. |
| 10 | Data loss risk. | Palace directory is a git repo tracking the 909 MB SQLite file; a `git checkout` lost 6 days of data on 2026-09-22. |

### 0.2 What Alice is

An open-source (MIT), local-first, long-term memory system for developers who work with AI coding agents. One memory, shared by every agent on the machine, that never blocks the agent, stays clean and correct over time, and can be inspected and managed through a UI.

- **Target users:** developers using Claude Code, Codex CLI, Cursor, Windsurf, OpenCode and Gemini CLI across many projects.
- **Business model:** fully open source, no paid tier.
- **Relationship to MemPalace:** new brand and a simpler model, inspired by MemPalace (MIT). Alice imports MemPalace palaces and offers an opt-in `mempalace_*` MCP compatibility layer so existing prompts keep working.
- **Memory creation model:** raw session transcripts are captured automatically (searchable on demand), and distilled into short knowledge memories that form the primary layer.

### 0.3 Success criteria (whole product, v1)

1. An agent never waits on memory: `remember` acknowledges in < 50 ms (p95); `recall` returns in < 200 ms (p95) at 1M chunks.
2. One install command makes every supported agent on the machine use Alice.
3. The owner's 117k-drawer palace imports completely, and search quality on a gold query set beats MemPalace.
4. The UI shows the memory graph and allows managing memories, reviews, settings and sync.

### 0.4 Non-goals for v1

- Multi-user team server, accounts or permissions (git sync covers multi-machine use).
- Hosted cloud service.
- Code search / repository indexing (Alice remembers what agents learned, not the code itself).
- Encryption of synced files (planned after v1, likely `age`).
- Mobile clients.

---

## 1. Decomposition

| Sub-project | Contents | Spec |
|---|---|---|
| **SP1 Core** | Daemon, storage, search, knowledge graph, write pipeline, Judge (heuristic + Jev), maintenance, settings API, git sync, MCP/CLI/HTTP interfaces, MemPalace importer and compat layer | This document |
| **SP2 Capture & Integrations** | Transcript readers per agent, hooks/plugins/skills, `alice setup` auto-detection, distillation pipeline, capture gating, additional Judge/LLM providers (Ollama, `claude -p`, `codex exec`, OpenAI-compatible) | Separate spec |
| **SP3 App** | Web UI (graph explorer, memory manager, review inbox, settings, sync, health), then a Tauri desktop shell | Separate spec |

SP1 is built first and is usable on its own as a MemPalace replacement (agents call `remember` explicitly; MemPalace data is imported).

---

## 2. Architecture

### 2.1 Process model

```
 Claude Code / Codex / Cursor / Windsurf / OpenCode / Gemini CLI
   ├─ MCP ── `alice mcp` (stdio shim) ───────┐
   │         or http://127.0.0.1:<port>/mcp ─┤
   └─ hooks ─ `alice hook <event>` (SP2) ────┤
 Terminal ── `alice recall …` ───────────────┼──► alice daemon (one per OS user)
 Browser UI ─ http://127.0.0.1:<port>/ (SP3) ┘      ├─ HTTP: JSON API + MCP (Streamable HTTP) + SSE
                                                     ├─ Writer thread (sole writer, short transactions)
                                                     ├─ Workers: embed, index, judge, import, sync
                                                     ├─ Scheduler: backups, checks, cleanup
                                                     └─ SQLite (WAL) + FTS5 + HNSW vector index
```

### 2.2 Rules

1. **One binary.** `alice` contains the daemon, CLI, MCP stdio shim and hook handler as subcommands.
2. **The daemon is the only process that opens the database.** All clients go through it.
3. **Heavy work never holds the writer.** Embedding, judging, importing, syncing and parsing run in worker threads and hand small batches to the writer thread. The writer only executes short transactions (target < 5 ms each, batched group commit). A large ingest is written in byte-bounded slices (≤ 64 KB of text and ≤ 200 chunks per transaction), so a `remember` queued behind it waits for one slice at most.
4. **Hooks never block and never lose data.** If the daemon is unreachable, the client writes the request to `~/.alice/spool/<ulid>.json` (one file per request, written to a temp file, fsynced and renamed into place), triggers a detached daemon start, and returns. The daemon drains the spool on start-up and on a 5 s timer. Text is redacted (§5.3) before it is written to the spool, and an entry that cannot be accepted yet because project settings are in a fail-closed state (§6) stays in the spool for the next drain; only a configured `block` mode deletes a refused entry.
5. **Auto-start.** The first client that cannot reach the daemon starts it detached. A lock file (`~/.alice/daemon.lock`, OS advisory lock) guarantees a single instance. `alice service install` registers a launchd agent `dev.alice.daemon` (macOS), a systemd user unit `alice.service` (Linux) or a logon scheduled task `dev.alice.daemon` (Windows) for always-on operation (§8.3). *As built:* the Windows task, and the Windows liveness check of a daemon through `daemon.pid`, are covered by unit tests and a plan test but have not been tried on a real Windows machine.
6. **Local security.** The daemon binds to 127.0.0.1 on a random free port, written with the pid and a per-start instance id to `~/.alice/daemon.json`. Every request carries a bearer token read from `~/.alice/token` (mode 0600, regenerated on `alice token rotate`). Before sending the token, a client proves it is talking to that daemon: `GET /api/v1/ping?nonce=<random>` must answer with the instance id from `daemon.json` and `HMAC-SHA256(token, "<nonce>:<instance>")`, so a stale `daemon.json` pointing at a port another process now uses never receives the token, and a proof relayed from another daemon does not vouch for a stale instance. The server rejects a request if its `Host` is not `127.0.0.1:<port>` or `localhost:<port>`, or if it carries an `Origin` other than the daemon's own origin (prevents DNS rebinding and cross-site requests). The UI gets a session through a one-time URL printed or opened by `alice ui`.
7. **Two MCP transports.** Clients that support Streamable HTTP connect to `/mcp` directly (with the token header). Others launch `alice mcp`, a stdio shim that proxies to the daemon (auto-starting it); target shim start-up < 20 ms.

### 2.3 Filesystem layout

`ALICE_HOME` (default `~/.alice` on all OSes):

```
alice.db            SQLite database (source of truth)
vectors/            rebuildable vector indexes, two files per embedding model:
                    <model>.memories.usearch (HNSW) and <model>.chunks.bq (flat sign-bit codes);
                    <model> is the model id with every character a file name cannot hold
                    replaced by "_" (a short hash is added when that changed the id)
models/             downloaded embedding models
spool/              pending requests (<ulid>.json) written while the daemon was down or read-only
backups/            SQLite snapshots (0600, in a 0700 directory), named
                    alice-<stamp>.db (scheduled), alice-manual-<stamp>.db,
                    alice-pre-migration-v<N>-<stamp>.db and alice-before-restore-<stamp>.db,
                    <stamp> = UTC yyyymmddThhmmssZ
logs/               daemon logs (rotated, never contain secrets)
config.toml         user configuration
secrets.toml        API keys (0600); secrets.toml.lock beside it serialises writers
daemon.json         {pid, port, instance, version, started_at}
daemon.lock
daemon.pid          {pid, start time}; where a held lock file cannot be read (Windows)
daemon.starting     {pid, step, file, done, total} while a daemon starts (e.g. the model download)
daemon.error        {pid, at, error}: why the last start failed; cleared when a daemon serves
restore.lock        present while `alice restore` runs (§5.6)
token               API token (0600)
```

### 2.4 Rust workspace

```
crates/
  alice-core/     domain model, store (SQLite), search, judge (heuristic, Jev), redact, settings schema, vector indexes; sync and import arrive in M3/M4
  alice-embed/    embedding providers (ONNX Runtime local with a compact tokenizer; Ollama, OpenAI-compatible, Voyage over HTTP)
  alice-daemon/   HTTP server, MCP server, workers, scheduler, service layer
  alice/          the `alice` binary: CLI, MCP stdio shim, hook handler, daemon entry
```

Key dependencies (versions pinned in the implementation plans): `rusqlite` (bundled, FTS5), `usearch` (built without NumKong, §4.1), `ort` (ONNX Runtime used directly; `fastembed` remains a dev-dependency of `alice-embed` only, as the reference its equivalence test checks against, §4.2), `rmcp` (official MCP Rust SDK), `axum` + `tokio`, `serde`, `schemars`, `ulid`, `clap`, `tracing`, `reqwest` (rustls) for the HTTP providers and Jev. `keyring` sits behind the optional `keychain` feature of `alice-daemon`, which the `alice` binary enables, and is used only when `secrets.backend = "keychain"` (§6).

---

## 3. Data model

### 3.1 Two tiers

| Tier | Unit | Purpose | Searched |
|---|---|---|---|
| **Knowledge** (primary) | `memory`: short, self-contained | facts, decisions, bug fixes, how-tos, preferences, warnings, status, notes, summaries, diaries, handoffs | Always |
| **Raw** (evidence) | `source` → `chunk` | session transcripts, files, imported verbatim drawers | When `include_raw` is set, or as a labelled fallback when knowledge results are weak |

### 3.2 Concepts

- **Project.** Detected from the client's working directory: git root → normalized remote URL (e.g. `github.com/icueth/alice`) as stable key; slug from the repo name. Without git, the directory basename. Slugs are canonical kebab-case; `project_aliases` maps every variant (`pg_ai`, `PG-AI`, `wing_pg_ai`) to one project. Reserved project `global` holds user-wide preferences and agent-agnostic knowledge.
- **Topic.** Free-form tags, many per memory, normalized to kebab-case. Replaces MemPalace rooms.
- **Memory kinds:** `fact`, `decision`, `bugfix`, `howto`, `preference`, `warning`, `status`, `note`, `summary`, `diary`, `handoff`.
- **Memory status:** `pending` (written, background steps not finished), `active`, `superseded`, `archived`, `trashed` (purged after 30 days).
- **Author** is metadata: `{agent, model, session_id}`. Agents never create containers; a subagent's diary is a `diary` memory in the current project.
- **Provenance:** a memory links to the raw chunks it was distilled from.
- **Links** between memories: `relates_to`, `supersedes`, `contradicts`, `derived_from`, `duplicate_of`.
- **Knowledge graph:** `entity` (name, type, aliases); `relation` (subject, predicate, object entity or literal, `valid_from`, `valid_to`, source memory); `predicate` vocabulary with descriptions and canonical forms; `mention` (memory ↔ entity), which drives the graph view.

### 3.3 Schema (SQLite)

```sql
projects(id TEXT PK, slug TEXT UNIQUE, name TEXT, git_remote TEXT, root_path TEXT, created_at TEXT)
project_aliases(alias TEXT PK, project_id TEXT)

memories(
  seq INTEGER PK AUTOINCREMENT,  -- rowid; FTS rows and vector keys use it, so it is never reused
  id TEXT UNIQUE,            -- ULID
  project_id TEXT, kind TEXT, title TEXT, body TEXT,
  importance INTEGER,        -- 1..5, default 3
  status TEXT,               -- pending|active|superseded|archived|trashed
  content_hash TEXT,         -- sha256 of normalized (project, kind, title, body)
  author_agent TEXT, author_model TEXT, author_session TEXT,
  created_at TEXT, updated_at TEXT, hlc TEXT,
  access_count INTEGER DEFAULT 0, last_accessed_at TEXT,
  legacy_id TEXT,            -- e.g. MemPalace drawer id
  embed_state TEXT,          -- pending|done|retry|skipped_cloud (held back by the cloud policy, §4.2)
  judge_state TEXT,          -- pending|retry|done|skipped (imports are never judged)
  kind_explicit INTEGER DEFAULT 1,      -- v3: 0 = the kind was defaulted to note; the judge may set it
  project_defaulted INTEGER DEFAULT 0,  -- v3: 1 = in `global` only because no project was known
  judge_attempts INTEGER DEFAULT 0,     -- v3: judgements that count toward giving up
  judge_unavailable INTEGER DEFAULT 0,  -- v3: failures only because the judge could not be reached
  judge_next_try_at TEXT                -- v3: a `retry` row is judged again from here
)
memory_topics(memory_id TEXT, topic TEXT, PRIMARY KEY(memory_id, topic))

sources(id TEXT PK, project_id TEXT, type TEXT,  -- session|file|import
        agent TEXT, uri TEXT, session_id TEXT, started_at TEXT, ended_at TEXT, legacy_ref TEXT)
chunks(seq INTEGER PK AUTOINCREMENT, id TEXT UNIQUE, source_id TEXT, project_id TEXT,
       ord INTEGER, text TEXT, content_hash TEXT, authored_at TEXT, created_at TEXT,
       embed_state TEXT, legacy_id TEXT, UNIQUE(source_id, ord))
provenance(memory_id TEXT, chunk_id TEXT, PRIMARY KEY(memory_id, chunk_id))
links(from_id TEXT, to_id TEXT, type TEXT, created_by TEXT, created_at TEXT,
      PRIMARY KEY(from_id, to_id, type))

entities(id TEXT PK, name TEXT, type TEXT, created_at TEXT)
entity_aliases(alias TEXT PK, entity_id TEXT)
predicates(name TEXT PK, description TEXT, canonical TEXT NULL)  -- canonical NULL = is canonical
relations(id TEXT PK, subject_id TEXT, predicate TEXT,
          object_entity_id TEXT NULL, object_literal TEXT NULL,
          valid_from TEXT, valid_to TEXT NULL, source_memory_id TEXT, created_at TEXT, hlc TEXT)
mentions(memory_id TEXT, entity_id TEXT, PRIMARY KEY(memory_id, entity_id))

embeddings(owner_type TEXT, owner_seq INTEGER, model TEXT, dim INTEGER, vector BLOB,  -- f16
           PRIMARY KEY(owner_type, owner_seq, model))

events(id TEXT PK, ts TEXT, hlc TEXT, actor TEXT, action TEXT,
       target_type TEXT, target_id TEXT, before_json TEXT, after_json TEXT,
       group_id TEXT)                                -- v2: one group per change that touches several rows
review_items(id TEXT PK, type TEXT, payload_json TEXT, confidence REAL,
             status TEXT, created_at TEXT, resolved_at TEXT, resolved_by TEXT, resolution TEXT,
             dedupe_key TEXT)                        -- v2: unique among open items
judge_cache(request_hash TEXT PK, provider TEXT, model TEXT, response_json TEXT, created_at TEXT)
cost_ledger(day TEXT, provider TEXT, calls INTEGER, input_tokens INTEGER, output_tokens INTEGER, usd REAL,
            PRIMARY KEY(day, provider))
sync_state(key TEXT PK, value TEXT)
schema_migrations(version INTEGER PK, applied_at TEXT)

-- FTS5, contentless (content='', contentless_delete=1), rowid = memories.seq / chunks.seq;
-- rows are written and deleted by the store together with the row they index:
--   memories_fts  tokenize='unicode61 remove_diacritics 2'  columns: title, body, topics, code_tokens
--   memories_tri  tokenize='trigram'                        columns: title, body
--   chunks_fts    tokenize='unicode61 remove_diacritics 2'  columns: text, code_tokens
--   chunks_tri    tokenize='trigram'                        columns: text
```

Migrations are forward-only, numbered, and run at daemon start inside a transaction after an automatic pre-migration backup (`alice-pre-migration-v<N>-<stamp>.db`, §5.6).

*As built (M2).* **Schema v2** (history and the review inbox): `events.group_id` (a supersede, a judge decision or a merge shares one group, and undo reverts the whole group, §5.5), `review_items.dedupe_key` with a unique index over open items (one open item per question), and the indexes `events_target`, `events_ts`, `events_group`, `events_actor` and `review_status`. **Schema v3** (the judge and maintenance): the five `memories` columns marked v3 above, the partial index `memories_judge` over rows whose `judge_state` is not `done`, and the maintenance indexes `chunks_created` and `judge_cache_created`. v3 was edited in place during M2 (`judge_next_try_at` arrived late) because no release had shipped it. Memories written before the judge existed are marked `done` by the migration, not judged after the fact. A `meta(key, value)` table holds small daemon state (the embedding model in force, re-embed cursors, last runs of maintenance jobs).

---

## 4. Storage and retrieval

### 4.1 Storage

- SQLite in WAL mode; `synchronous=NORMAL`, `busy_timeout=5000`, `foreign_keys=ON`, no memory map (`mmap_size=0`: each connection's map counts towards RSS separately, and reads through the OS file cache measured the same speed), allocation statistics off (they serialise every allocation behind one mutex).
- One writer connection (writer thread, 64 MiB page cache); a pool of at most 8 read connections (16 MiB page cache each) for concurrent queries. Reads beyond 8 wait for a free connection, which bounds memory however many requests arrive at once.
- Vector indexes, two per model, both fully rebuildable from the `embeddings` table (f16 vectors, the source of truth):
  - **Memories**: a `usearch` HNSW (cosine, int8 quantization) in `vectors/<model>.memories.usearch`. usearch is built without its NumKong SIMD kernels (`default-features = false`) on every OS, so all platforms run one code path and Windows links; the portable distance code costs a few milliseconds at large memory counts (`docs/benchmarks/m2.md`, Windows). A memory search walks only this graph, so it never pays for the (much larger) chunk part. Candidates are rescored by exact cosine from the stored f16 vectors before the relevance floor is applied (§4.4).
  - **Chunks** (*as built, M2*): a flat index of sign bits in `vectors/<model>.chunks.bq` (`alice_core::bq`), not a graph: one bit per dimension (whether the coordinate is above the mean vector), 48 bytes of code and 8 of key per 384-d vector (53.4 MB at 1M chunks against 508 MB for the M1 int8 HNSW). A raw-tier vector search (1) takes the `max(4,000, 32·√N)` codes nearest to the query by Hamming distance, (2) re-ranks that shortlist in RAM against the float query, and (3) reads the best `max(k, 400)` of them from SQLite and rescores them by exact cosine from their stored f16 vectors. Measured recall@10 against the exact top 10 over real passages (this repository's files and the cargo registry): 0.994 at 6,000 passages, 0.995–0.996 at 24,000 and 0.988 (rebuilt) to 0.990 (filled by inserts) at 96,000. The ignored test `crates/alice-embed/tests/binary_chunk_recall.rs` gates recall@10 ≥ 0.97 at ≥ 96,000 passages; the figures and the cost of each choice are in `docs/benchmarks/m2.md`. Cost: a raw-tier vector query scans the codes twice (~48 MB each at 1M chunks, ~9 ms of CPU) and reads a few hundred rows.
  - Index files are serialised in memory and read and written by Rust (usearch never opens a path, which fails for names that are not ASCII on Windows or not UTF-8 on Unix); each part is written atomically (temp file, sync, rename) and an unchanged part is not rewritten. On start-up and daily (the `index_check` job) the index counts are compared with the table; on mismatch or load failure the index is rebuilt on a background thread while search uses the lexical lists only (about 6 s for both parts at 1M chunks). A leftover M1 `<model>.chunks.usearch` is deleted on the first save.

### 4.2 Embeddings

- Default: `multilingual-e5-small-q8`, the int8 ONNX build of `intfloat/multilingual-e5-small` (384-d) from `Xenova/multilingual-e5-small` at a pinned revision: `onnx/model_quantized.onnx` (118 MB) plus `tokenizer.json` (17 MB) and three small config files, with mean pooling and `query: ` / `passage: ` prefixes. Good Thai and English quality, no API key.
- *As built (M2):* **ONNX Runtime is used directly** (`ort` 2.0.0-rc.13, linked statically) with a **compact Unigram tokenizer** in `alice-embed` (`tokenizer.rs`) instead of fastembed and the Hugging Face `tokenizers` crate. The tokenizer keeps the 250k pieces in one hash map (~21 MB live) where the `tokenizers` trie cost ~250 MB; a `tokenizer.json` whose pipeline differs from the one implemented is refused. It reproduces the reference exactly: `crates/alice-embed/tests/onnx_equivalence.rs` (ignored, real model; fastembed stays a dev-dependency only for this check) finds 0 token-id mismatches over 861 texts and a worst cosine of 1.000000 against the vectors M1 stored, so stored vectors stay valid and the model id is unchanged. One difference: literal special-token text such as `</s>` inside a passage is tokenised as text, where Hugging Face emits the special id. The ONNX session runs with the **CPU arena and memory patterns off** (ONNX Runtime would otherwise keep its largest call's working memory for good; query latency is unchanged), reads the model from its file, and uses `min(4, cores)` intra-op threads (on the one machine measured a background daemon answers a query about 25% faster with 4 threads than with all 14 cores and spends 36% less CPU on bulk embedding, at 107–112 against 133–136 passages/s; `docs/benchmarks/m2.md`). One loaded instance costs ~348 MB (fastembed: ~600 MB), so the daemon keeps one, shared by the background worker and queries: passages are embedded in batches of at most 1,024 padded tokens, counted with the model's tokenizer (32 texts at most) and the model is handed to a waiting query after each batch, so a query waits for one batch at most (~115 ms for two 512-token texts on an M4 Pro).
- *As built (M2):* **idle unload.** `embedding.idle_unload` (default `10m`, `0` = never; restart-only) drops the ONNX session after that long unused. A query that arrives while the model is gone does not wait: it fails at once with `MODEL_LOADING`, search answers from the lexical lists alone (logged at debug level, not as a failure) and the reload starts in the background; the embedding worker's passages wait for the reload (the real model reloads in tens to hundreds of milliseconds with the file in the OS cache). In the daemon the unload gave back about 190 MB of footprint (356 → 163 MB with one memory).
- Providers: `local` (ONNX), `ollama`, `openai` (any OpenAI-compatible endpoint) and `voyage`. *As built (M2):* `embedding.provider`, `embedding.model` and an optional `embedding.endpoint` choose one; `openai` and `voyage` need a key (`alice config set-secret embedding_api_key`, or `ALICE_EMBEDDING_API_KEY`), which is sent only as a bearer header to that provider's endpoint, never to another host. Vector ids are `<provider>:<model>` for HTTP providers and the plain model id for the local one; index file names are sanitised (§2.3). A request carries one batch (callers clamp to the provider's maximum, `embedding.batch_size`, 1–256) and is made once: only the start-up probe, which carries no user text, is retried (on 429, 5xx and timeouts); redirects are not followed and no environment proxy is used; a recall waits at most 2 s for its query vector and then answers from words (the next 30 s do not ask at all). The daemon never calls a provider while starting: the model recorded as in force is taken up with a stand-in embedder (stored vectors stay searchable; new text waits in the queue) until a control thread has built the real one (retrying every minute).
- *As built (M2):* **what leaves the machine.** An embedder is local when the provider is `local` or its endpoint host is `localhost` or a loopback address (a local Ollama); anything else is remote. A remote embedder gets a memory, chunk or recall query only if the cloud policy (§6: `privacy.cloud_denied_projects`, a project's `.alice.toml`) allows the project, asked again before every send attempt, and after the project's secret patterns have been applied to a query. Held-back memories and chunks get `embed_state = skipped_cloud`: searchable by words, not a backlog, queued again when the policy changes (checked every five minutes and at once after a change of `embedding.*` or `privacy.*`). Details: `docs/benchmarks/m2-embedding.md`.
- Each vector records its model. *As built (M2):* a change of provider, model or endpoint applies without a restart: (1) the model in force keeps serving; (2) a background re-embed (phase 1, resumable from cursors in `meta`, waiting for quiet) fills the new model's vectors and its index files; (3) when everything has a vector (phase 2) the embedding worker is paused for a moment, a final pass embeds what was written meanwhile, and the model, embedder and index are replaced together atomically; (4) the old model's vectors and index files are deleted later by the daily `model_purge` job. A failure leaves the old model in force and says why (`embedding_error` in health). One re-embed runs at a time; a change made while the daemon is stopped is picked up at the next start from `meta`. Switching to a remote provider is a loosening of a protection (§6).
- Model files are downloaded on first use to `models/<model>/` with size and SHA-256 verification (`HF_ENDPOINT` selects a mirror); `alice model fetch` pre-downloads for offline machines.
- Embedding completion is compare-and-set: the worker stores a vector only if the row's `content_hash` still equals the one it read with the text, so an edit that lands while the old text is being embedded is never marked done with the old vector.
- Local embeddings need an ONNX Runtime prebuilt library: Apple Silicon macOS, glibc Linux (x86_64, aarch64) and Windows x64. Intel macOS and musl Linux have none; they run with an HTTP embedding provider. *As built (M2):* the Windows build links (usearch without NumKong, §4.1) and the real model is verified on Windows by the manually started `windows-real-model` CI workflow (the data directory under a non-ASCII path); the same model gives slightly different similarities on x86-64 than on ARM (up to 0.02), which is why the relevance rescue floor carries a 0.015 allowance (§4.4); measured there, the highest of the 60 unrelated gold-set pairs scores 0.792 (0.786 on Apple Silicon), so 0.013 of the allowance is left and no unrelated pair reaches the floor.

### 4.3 Lexical search

- FTS5 `unicode61` for space-delimited languages. English stopwords, including the fragments contractions split into at an apostrophe (`it's` → `it`, `s`; `don't` → `don`, `t`; straight or curly), are dropped from word queries whenever the query has other words (a query of stopwords only keeps them), so a lexical match is a content-word match; regular English inflections of query words are added (`replies` → `reply`, `deploy` → `deploys`, `deployed`), since `unicode61` does not stem. Every term is quoted, so FTS5 syntax in a query is searched as literal words.
- FTS5 `trigram` for Thai, CJK and substring matches (Thai has no word spaces, so word tokenizers miss it). *As built (M2):* a hyphenated or dotted number — a date, a version, an IP address, a phone number or a range (`2026-10-14`, `1.13.0`, `10.20.30.40`, `081-234-5678`, `10-20`) — is searched as one more bounded trigram term, so an exact query finds the text that has the whole number ahead of texts that merely contain its parts.
- `code_tokens` column: at index time, identifiers and paths are expanded (`HostLinkReconnect` → `host link reconnect`, `writer_lease` → `writer lease`, `internal/teacher/jev/client.go` → path segments and stem). Each name in a token (split at `/ . :`) also yields the joined runs of its consecutive words (split at `_`, `-` and camel case; at most 12 words, runs of up to 6), so every written form of an identifier carries its joined token: `self.lease_token_frame`, `store/lease_token_frame.rs`, `crate::daemon::lease_token_frame()`, `LEASE_TOKEN_FRAME_LIMIT`, `lease-token-frame` and `LeaseTokenFrame` all hold `leasetokenframe`. Kebab-case counts as an identifier when every hyphen-separated part has a letter (`well-known`, `lease-token-frame`; not dates or ranges such as `2026-10-14` or `15-45`). Exact identifier/path queries also get a dedicated exact-match candidate list, searched by the whole joined identifier and by the joined form of each multi-word name in it (the runs of 2–6 words are index-side only). The word lists search an identifier's parts as typed and never its joined forms, so a long identifier cannot push a query's other words past the 32-term cap.

### 4.4 Ranking pipeline

1. **Candidates:** vector top 100; `unicode61` BM25 top 100; trigram top 100 (always for non-space scripts or queries of ≤ 2 pieces, otherwise when the other lists are short; ASCII pieces are searched by substring only in queries of ≤ 2 pieces, counted after splitting at punctuation, so a path, URL or dotted name of many pieces sends none); exact identifier/path matches; in the knowledge tier only, a `LIKE` scan for non-ASCII terms shorter than a trigram (1–2 code points, such as Thai `จำ`). In the raw tier such a word of a script written without spaces has no lexical list (only the vector list can find it). With filters (project, kind, topic, dates) each list takes 400. Only the first 2,000 characters of a query are searched and embedded. Raw-tier lists bound their cost per term (ranking every match of a word found in 40% of 1M chunks took ~300 ms). In the word and identifier lists, every term that has a match gets its **own share** of the budget, max(k, 5,000 / number of such terms) rows (k is 100, or 400 with filters), and reads its newest matches up to that share; a chunk scores the sum of the BM25 parts of every term that read it, and the best k are kept. Rows read per list are at most max(5,000, terms × k): 5,000 for a few terms, up to 6,400 for the word list's 64 terms (32 words and 32 variants), or 25,600 with filters. The raw word list does not search identifiers holding `_` as phrases of their words (BM25's IDF for a phrase of common words reads every match of each word with positions, 36–50 ms per identifier at 1M chunks); they are found as one token in the identifier list, in every written form, and their words stay in the word list. In the raw tier every word of a script written without spaces (Thai, Lao, Khmer, Myanmar, CJK, ...: any word holding a character of those Unicode ranges), with combining marks or without, is searched through the trigram list only: unicode61 splits Thai at combining marks (7 of the bench's 10 common Thai words: `ตัดสินใจ` becomes `ต`, `ดส`, `นใจ`, a phrase of common fragments whose IDF read 25–31 ms at 1M chunks), and a rare and a common word of one query must be ranked by the same list (with only marked words left to the trigram list, a rare marked word beside a common unmarked one lost its places in the fused ranking). Other non-ASCII words (accented Latin, Cyrillic, Greek, ...) stay in the word list; the knowledge tier's word list keeps every word. Every lexical list splits a query at the same separators: whitespace and ASCII and Unicode punctuation and symbols (`/ , : ; “ ” ‘ ’ … — –`), `_` excepted, so a word touching punctuation is still searched whole. In the trigram list, a non-ASCII term that has a match reads its newest max(k, 5,000 / number of non-ASCII terms that have a match) matches (a term without a match takes no share, as in the word and identifier lists), and an ASCII term its newest k substring matches (as a whole word it has its own share in the word list, as an identifier in the identifier list); a chunk scores Σ 1 / (each matching term's count, capped at what it read), and ties are taken term by term (BM25's IDF needs every match of each trigram phrase, 60–400 ms at 1M chunks). **Guarantee:** no term can crowd another out of the raw tier, because every term is read up to its own share; a term with at most its share of matches is ranked over all of them; matches beyond a term's share (and an ASCII term's substring-only matches beyond its newest k) are not read, newest first, so recall for very common terms is not exhaustive. A share costs ~5–16 ms at 1M chunks for a single word; with identifiers and the words of scripts written without spaces out of the raw word list, phrase terms are left only for words of other scripts that unicode61 splits at combining marks (Devanagari vowel signs, for example).
2. **Fusion:** Reciprocal Rank Fusion, k = 60.
3. **Boosts** (multiplicative, all configurable):
   - tier: knowledge 1.0, raw 0.6
   - project: current 1.3, `global` 1.1, other 1.0
   - recency: `0.7 + 0.3 · exp(−age / half_life(kind))`, half-lives: `status` 7 d, `handoff` 14 d, `diary` 30 d, `note` 90 d, `fact`/`howto`/`bugfix`/`summary` 180 d, `decision`/`preference`/`warning` no decay
   - importance: `0.9 + 0.05 · importance`
   - usage: up to +10 % by `log(1 + access_count)`
   - status: `superseded` 0.3, `archived` 0.5 (still retrievable)
4. **Precise mode (optional):** the Judge scores the top 20 for relevance in one batched call and results are re-ordered by P(relevant). *As built (M2):* `mode: precise` (`alice recall --precise`) needs a model judge (`judge.provider = jev`) and a project the cloud policy allows; it waits at most `judge.timeout_ms`, and when the judge cannot grade (the heuristic, an outage, an exhausted budget, a denied project, too slow) the search order stands and the answer says it was not re-ranked.
5. **Raw fallback:** a result is *relevant* if it has a lexical match (BM25, trigram or exact) or a cosine similarity ≥ `search.relevance_floor` (default 0.82 for `multilingual-e5-small-q8`, checked in M1 with the real model on the gold set: as the int8 index reports similarity, every gold query–memory pair scores ≥ 0.841 and every unrelated pair ≤ 0.771; `crates/alice-embed/tests/real_search_quality.rs`). *As built (M2, Task 13):* for memories the vector test is three-part, on the exact cosine of the stored vectors: a similarity ≥ `search.relevance_sure` (default 0.825) is relevant; below it, one ≥ `search.relevance_floor` (now the *rescue* floor, default 0.805) is relevant only if it is within `search.relevance_margin` (default 0.015; 0 = off) of the query's best vector match. `relevance_sure = relevance_floor` gives the old single floor; a floor above 0.825 without a `relevance_sure` of its own raises the sure floor with it. Transcript chunks keep a fixed 0.82. Values and evidence: `docs/benchmarks/m2-jev.md`. If `include_raw` is false and fewer than `search.raw_fallback_min_results` (default 3) knowledge results are relevant, relevant raw results are appended and labelled `raw`.
6. **Related:** up to 5 items one hop away from the top results through links and shared entities, returned separately.

### 4.5 Output for LLMs

- Default text format: one block per result with id, kind, project, date, title, a 1–2 line snippet, and `matched: vector|words|code|exact`. Full bodies for the top results while the token budget allows.
- `max_tokens` (default 1,500) caps the response; the server packs results to fit and reports how many were omitted.
- `format: "json"` returns structured results.
- Each returned memory's `access_count` is incremented asynchronously.

### 4.6 Performance budgets

| Operation | Target (p95) |
|---|---|
| `remember` acknowledgement | < 50 ms |
| `recall` fast mode, 1M chunks | < 200 ms |
| `context` briefing | < 150 ms |
| `alice mcp` shim start-up | < 20 ms |
| Daemon RSS at 1M chunks | < 800 MB |

*M2 on the final head (met; `docs/benchmarks/m2.md`, Apple M4 Pro, one run each, real model loaded, 1M chunks with prefilled vectors):* daemon peak RSS **564 MB** and peak physical footprint 585 MB, over a run that loads the model, embeds 256 new memories and serves 4 bursts of 64 concurrent recalls with no errors; `remember` p95 13.4 ms, `recall` p95 15.6 ms, `include_raw` p95 119 ms (106 ms under a filter, 69 ms for a rare identifier, 48 ms for a rare Thai word, 51 ms for the raw fallback), `context` p95 0.50 ms, `alice mcp` start-up 4.8 ms (p50) and 5.2 ms (p95) from spawn to the `initialize` reply. At 100k chunks with the real model the daemon peaked at 534 MB RSS, `remember` 4.4 ms, `recall` 6.2 ms, `include_raw` 26 ms. The memory budget M1 missed was met by the sign-bit chunk index (§4.1) and the direct ONNX Runtime embedder (§4.2); every number is one run on a machine that was lightly loaded (1-minute load average 2.6–4.5), and the 1M figures use synthetic vectors under the real model's id (the vector index costs the same whatever its values).

M1 measurements (before M2's memory work) are in [docs/benchmarks/m1.md](../../benchmarks/m1.md): every latency budget is met at 1M chunks (`include_raw` recall p95 139 ms; 123 ms under a filter, 118 ms for a rare identifier, 22 ms for a rare Thai word, 70 ms for the raw fallback; 177 / 162 / 148 / 31 / 103 ms in a second run). Under heavy contention from other processes one run exceeded it (`include_raw` 267 ms, identifier 262 ms); an A/B against the previous code under the same load showed the current code as fast or faster, so the margin under load is thin rather than the code slower. The memory budget (RSS) was not met in M1: without the embedding model the daemon at 1M chunks held 666–784 MB RSS and peaked at 722–860 MB across runs (footprint up to 862 MB); with the local model it is projected at ≈1.4 GB RSS steady and ≈1.6–1.7 GB RSS at peak (physical footprint ≈1.4–1.5 GB steady, ≈1.55 GB peak); one model instance alone is ~600 MB. At 100k chunks with the model the peak RSS is 910–1,003 MB (footprint ~0.85 GB).

---

## 5. Write path, Judge and maintenance

### 5.1 Write path (`remember`, hook deliveries, imports)

1. **Normalize:** resolve the project via aliases, normalize topics, default `kind` to `note` (refined later by the Judge if enabled).
2. **Redact secrets** (§5.3) before anything touches disk.
3. **Exact dedup:** if `content_hash` exists in the project, return the existing id with `status: duplicate`. This makes retries idempotent. An optional client `idempotency_key` covers non-identical retries.
4. **Insert** the memory (status `pending`) and its FTS rows in one transaction; **acknowledge** with the id. The memory is immediately findable by lexical search.
5. **Background pipeline:** embed → add to vector index → Judge: compare with the 5 nearest memories in the project (§5.2) → link mentioned entities (alias match) → set status `active`. *As built (M2):* a memory becomes `active` only after its judge stage is over (judged, or activated unjudged after three failures in a row to judge or apply it); `pending` and `active` are treated alike by search, context and undo, so a pending memory is already findable and can be edited or undone. A memory that is in `global` only because no project was known carries `project_defaulted = 1` and may be routed to a project (§5.2); a memory whose writer gave a kind or a project (`kind_explicit`) is never changed by the Judge. Memories with `judge_state = skipped` (M3 imports) are never judged. New entities are not proposed by the Judge in M2 — Jev answers choice questions only, so no M2 provider can name one; mentions link known entities, and the graph grows through `assert_fact` until SP2.

Raw chunks follow steps 1–4 and then embed + index only.

### 5.2 Judge

A provider-neutral layer for decisions with calibrated probabilities.

```rust
trait Judge {
    fn classify(&self, state: &str, options: &[Choice]) -> Result<Distribution>;    // P over options
    fn score(&self, state: &str, rubric: &Rubric) -> Result<Score>;                 // value + confidence
    fn compare(&self, new: &Memory, existing: &[Memory]) -> Result<Vec<Relation>>;  // per pair: P(duplicate|updates|contradicts|unrelated)
}
```

**Providers in SP1**

- `heuristic` (default, offline): duplicate if cosine ≥ 0.97 in the same project; an "updates" candidate if the same project, kind and near-identical title are newer; kind from keyword rules; predicate match by normalized string similarity (Jaro-Winkler ≥ 0.9 after stemming). Heuristic results above the auto threshold apply only for duplicates; everything else goes to review. *As built (M2):* it is stricter than that: it **auto-applies only a duplicate whose text is the same after NFC normalisation and whitespace collapse** (`normalize::same_text`, the identity of an idempotent write: case and punctuation count, the kind is ignored) and a predicate that is identical after stemming. It never merges on cosine alone: a near-duplicate by cosine, an "updates" candidate and a project guess become review items at most; its keyword rules set no kind (only a model judge sets one); predicates that differ only in direction (`depends_on` / `depended_on_by`) are never mapped to each other without a person looking. A heuristic that stands in for a Jev that could not answer only suggests. It compares a new memory only with older active memories.
- `jev` (optional): TypeSafe Jev (`https://api.typesafe.ai/v1/systemone`), model pinned to `jev-1.13.0`, key from `secrets.toml` (`jev_api_key`, §6), the OS keychain when configured, or `TYPESAFE_API_KEY` (the environment wins). Choice questions only: the request carries a state, and per question a set of options with descriptions, and the answer holds the chosen option and probabilities.
  - Numbers are verbalized before sending (Jev reads words better than numbers): "yesterday", "two weeks ago", "importance high".
  - Choice options are shuffled, *as built* with a seed derived from the request itself, so an identical decision builds an identical body (and hits the cache) while different decisions see different orders. For `compare` decisions that may supersede, the same pair is asked twice in the same request, forward and as the converse relation (`outdated_by`), and the distributions averaged.
  - Responses are cached by SHA-256 of the exact request body. Retries (up to 3, backing off 0.5 s doubling to 16 s, or the server's `Retry-After` up to 30 s) only on 429, 5xx and timeouts; a connection error is not retried: the call fails at once as unavailable, the heuristic decides and the memory is marked `retry` (below). Rate ≤ `judge.jev.rate_per_sec` (default and maximum 10 requests/s). The client follows no redirect (a redirect would post the request, and the key, to another host).
  - Daily budget in USD (`judge.daily_budget_usd`, default 1.0, per UTC day) with a hard stop. *As built:* each call reserves an upper bound of its cost before it is made — the body's bytes times a tokens-per-byte ratio (1.5 to start, raised to the highest ratio Jev has reported) plus 300 tokens for Jev's own prompt, at $0.042 per million input tokens, output free — and settles to the reported usage afterwards, so the day's spend never passes the budget; a call that was sent but reported no usage keeps its reservation as its cost. A spend that cannot be read counts as an exhausted budget (fail closed; health says "cannot read today's Jev spend"). Every call is recorded in `cost_ledger`.
  - Never sends text of a project the cloud policy denies (`privacy.cloud_denied_projects`, a project's `.alice.toml`; §6): `SettingsHandle::cloud_allowed` is the only gate and is asked for every identity of every project a call covers; a degraded configuration says no, and a call that names no project is denied unless it is marked as carrying no project text. Redaction always runs first, with the rules of every project the request covers. A denied project is judged by the heuristic.
  - Measured on a synthetic 216-pair set (`docs/benchmarks/m2-jev.md`, one run, one author's labels, with the caveats stated there): relation accuracy 0.917 against 0.287 for the heuristic, kind 0.769 against 0.218, $0.0113 per 1,000 decisions; at `auto_apply_threshold` 0.85 all 80 applied decisions were right on that balanced set, which is consistent with, but does not establish, 95% precision on real traffic.

SP2 adds `ollama`, `claude-cli`, `codex-cli` and `openai-compatible` providers.

**Judge tasks in SP1**

| Task | Question | Action |
|---|---|---|
| Relation to existing | For each of the 5 nearest (*as built:* plus up to 3 more of the same kind whose titles are nearly the same, so an update worded differently from its predecessor is still compared): duplicate / updates / contradicts / unrelated | duplicate → the older memory survives and gains the new one's provenance; the new one becomes `trashed` with a `duplicate_of` link, and `get_memory` on its id returns the survivor; updates → new `supersedes` old (old becomes `superseded`); contradicts → `contradicts` link + review item |
| Kind | Which kind fits this memory? | Set `kind` when the writer did not (`kind_explicit = 0`); a failed classification is retried like a failed comparison |
| Project routing | Which existing project does this belong to? (only when cwd/roots are unknown, `project_defaulted`) | Set `project_id`, or a review item below the auto threshold; a project the cloud policy keeps from a model is never offered to one |
| Predicate normalization | Which existing predicate does this one mean? (inline in `assert_fact`, waiting at most 3 s, then the heuristic decides) | Map to canonical predicate or create a new one |
| Precise recall | How well does each result answer the query? | Re-rank |

**Thresholds:** auto-apply when P ≥ `judge.auto_apply_threshold` (default 0.85); create a review item when P ≥ `judge.review_threshold` (default 0.5); otherwise do nothing. If the Judge fails, times out or cannot be reached, the heuristic runs instead (its answers then only suggest) and the memory is marked `judge_state = retry` for a later pass. A Judge failure never fails a write. *As built (M2):* a judge action whose target changed after the judge read it (every memory the judge saw is stamped) becomes a review item instead of being applied. A `retry` memory is judged again after a back-off of 5 minutes that doubles with every failure up to 6 hours (the judge worker looks every 5 minutes, and the hourly `judge_retry` job does too, at `judge_next_try_at`); after 5 counted attempts it stops (`done`, left to the heuristic's result); a failure only because the judge could not be reached does not count as an attempt. A project the cloud policy denies and an exhausted daily budget are not failures: the heuristic decides and nothing is retried for them.

### 5.3 Secret redaction

- Built-in detectors (the name is what appears in `[REDACTED:<name>]` and in a refusal): `pem_private_key`, `jwt`, `aws_access_key` (`AKIA`/`ASIA`), `aws_secret_key`, `gcp_key_id` (a service-account `private_key_id`), `github_token` (`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`), `gitlab_token` (`glpat-`), `slack_token` (`xox[abprs]-`), `stripe_key` (`sk_live_`, `rk_live_`), `sendgrid_key`, `vault_token` (`hvs.`, `hvb.`, `hvr.`), `anthropic_key` (`sk-ant-`), `openai_key` (`sk-`, `sk-proj-`, `sk-svcacct-`), `url_credentials` (`scheme://user:pass@`), `password_assignment` (`password|passwd|secret|token|api_key|…` assignments, `--password value` style flags, and the Thai `รหัสผ่าน`) and `high_entropy` (strings ≥ 32 characters in assignment context). *As built:* redaction errs toward redacting: a snake_case value is not taken as evidence of code, and a value is kept as code only when it is a call or index, a dotted access of short plain identifiers, or has an accessor-verb prefix.
- Matches are replaced with `[REDACTED:<type>]`; text that is already redacted is left as it is. Mode `block` rejects the write instead, with a message naming the field and the detectors (never the secret): over HTTP the answer is 400.
- User-defined patterns (`{name, regex}`, shown as `[REDACTED:custom:<name>]`) and allowlists (regular expressions; a match that one of them matches completely is kept) in config. A project's `.alice.toml` may add patterns and switch on block mode (§6) but never weaken the global rules.
- *As built (M2):* redaction runs in the service layer before every write operation (the writer does not scan again); a write queued in the spool is redacted before it is written there; the before/after snapshots that events store are redacted, and an undo restores through the same check (block mode refuses a restore that holds a secret, redact mode cleans it); a Jev request body is redacted with the rules of every project it covers; purging a memory scrubs the snapshots of the events that target it. The daemon logs no memory text. Sync exports (M4) will apply it too.

### 5.4 Review inbox

Review items have a type (`supersede`, `contradiction`, `merge`, `predicate`, `project`, `sync_conflict`, and, *as built (M2)*, `undo`: an undo whose target changed since the event needs a question), a payload, a confidence and a status (`open`, `resolved`). They are resolved from the UI (SP3), `alice review [list|show|resolve <id> <choice>]`, `POST /api/v1/reviews/{id}/resolve` or the admin MCP toolset. Resolutions are recorded as events. Each type offers fixed choices:

| Type | Choices |
|---|---|
| `supersede` | `accept` (the new memory replaces the old one), `reject` (keep both) |
| `contradiction` | `keep_new` (it supersedes the old one), `keep_old` (the new one becomes superseded), `keep_both` (linked as contradicting) |
| `merge` | `merge` (the survivor stays, the others become its duplicates), `reject` |
| `predicate` | `map` (treat the new predicate as the existing one), `keep` |
| `project` | `move` (to the suggested project), `keep` |
| `undo` | `apply` (undo anyway), `dismiss` |
| `sync_conflict` (M4) | `keep_local`, `keep_remote` |

At most one open item exists per question (`dedupe_key`), so a judge or a weekly job asking again finds the open one; resolving a resolved item is an error that says so.

### 5.5 History and undo

Every mutation writes an `events` row with actor (`agent:<name>`, `user`, `judge:<provider>`, `sync`, `import`, and, *as built (M2)*, `system` for maintenance), action, target and before/after JSON; a change that touches several rows (a supersede, a judge decision, a merge) shares one `group_id`. `alice undo <event-id>` reverses an event when the target has not changed since; otherwise it creates an `undo` review item (§5.4) and changes nothing. Deletes move memories to `trashed`; trash is purged after `maintenance.trash_days` (30).

*As built (M2):*
- Undo reverts the **whole group** of the event it is given, newest first, and records its own events (an undo can itself be undone; the error for an event already undone names the event to undo instead).
- Only changes to memories, facts and predicates can be undone. These cannot: `undo` markers, `source.ingest` (one summary event per ingest request, not per chunk), `memory.purge`, `raw.retention`, `settings.change`, `secret.set` and `secret.delete`. A purged memory cannot come back (an error, not a review item).
- `pending` and `active` memories are one state for undo, as for search.
- A settings change that takes effect is recorded as a `settings.change` event naming the keys (never a value; protective keys that got weaker are named), a secret change as `secret.set` or `secret.delete`, so `alice history` shows who changed what.
- Events older than `maintenance.events_days` (365) are pruned, whole groups at a time (§5.6).

### 5.6 Maintenance scheduler

Runs in a low-priority worker when the daemon has been idle for `maintenance.idle_secs` (60 s; 10 s once the newest backup is older than two backup periods) and yields as soon as requests arrive: every job works in short slices (a writer slice stops after about 25 ms; deletes go 500 rows per write operation, FTS merges 500 pages), stops between slices when a request comes and continues in the next idle period. `maintenance.enabled = false` turns the scheduler off. A job that failed is due again after an hour (12 hours when the disk is full). `alice maintenance status` shows when each job last ran (kept in `meta`); `alice maintenance run <job>` runs one to its end.

| Job | Default frequency |
|---|---|
| `backup`: SQLite online backup; keep the newest of each of the last `backup.daily` (7) days and of each of the last `backup.weekly` (4) ISO weeks in `backups/` | Daily |
| `quick_check`: `PRAGMA quick_check` (a check interrupted by a request counts as paused, not failed) | Daily |
| `index_check`: vector index count check and rebuild if needed | Daily |
| `fts_optimize`: bounded FTS `merge` steps (500 pages per writer slice) over the four FTS tables | Weekly |
| `raw_retention`: raw tier (`forever` default; or by age, e.g. `90d`, counted from when Alice stored a chunk; or by size, e.g. `10GB`, counted in chunk text bytes) | Daily |
| `merge_suggestions`: similar-memory clusters (cosine ≥ `judge.cluster_similarity`, 0.92) → `merge` review items | Weekly |
| `trash_purge`: trashed memories older than `maintenance.trash_days` (30) | Daily |
| `events_prune`: events older than `maintenance.events_days` (365), whole groups, and old judge-cache entries | Daily |
| `model_purge`: vectors and index files of an embedding model that is no longer in force | Daily |
| `judge_retry`: put `retry` memories whose time has come back in the judge queue | Hourly |
| Spool drain (not a scheduled job: a timer of the daemon) | Every 5 s and at start-up |

*As built (M2):*
- **FTS merge steps instead of `optimize`** (Plan decision 32): `optimize` rewrites a whole index in one write transaction, which at 1M chunks would hold Alice's single writer for seconds to minutes and stall every `remember`. Merge steps leave an index with somewhat more segments than a full optimize (FTS queries a little slower until later merges), and a large index takes several idle periods to merge fully.
- **Backups.** `alice backup now|list` and the daily job make one online-backup step on a dedicated read-only connection (a consistent snapshot while readers and the writer go on), check it with `quick_check` and rename it into place, so a failed backup leaves no file. Names: `alice-<stamp>.db` (scheduled), `alice-manual-<stamp>.db`, `alice-pre-migration-v<N>-<stamp>.db` (taken before a migration) and `alice-before-restore-<stamp>.db`. Rotation never deletes the newest good backup; manual backups rotate separately (the newest 5 are kept whatever the day); pre-migration and before-restore copies are kept 30 days plus the newest of each. A backup needs room for a second copy, so the job checks free space first (refusing with "disk space: …" and changing nothing) and sweeps the temporary files of a killed copy. A backup holds the database and nothing else — never `secrets.toml`, the token or the vector files — and is private: files 0600 in a directory 0700 (Unix).
- **Retention limits:** a raw age below `1d` or a size below `1MB` is refused, as is `"0"` and an age over 100 years. The database file does not shrink when rows go. A purge removes a memory's FTS rows, vectors, topics, links, mentions, provenance and event snapshots; its words stay in the FTS segments until a merge rewrites them, and its text stays in older backups (about three weeks) and in pre-migration and before-restore copies (30 days).
- **The database file is never stored in git.**
- **Restore** is `alice restore [backup] [--yes]`, run by the CLI under the daemon lock, never through a daemon endpoint (no daemon can run while files are swapped; the CLI never opens `alice.db` itself). Order: find and verify the backup (a SQLite file that passes `quick_check`, with Alice's tables and a schema not newer than the build) and check free space before anything is stopped; confirm; take `restore.lock` (one restore at a time; clients spool writes at once and start no daemon meanwhile); stop an installed service unit (launchd `bootout`, `systemctl --user stop`, `schtasks /End`) and then the daemon; stage the backup beside `alice.db` and check the staged file; keep the current database as `alice-before-restore-<stamp>.db`; remove the vector index files (they are rebuilt at the next start); fold the WAL in and put `-wal`/`-shm` aside; one atomic rename; start the daemon (and the unit again, also when the swap failed). The invariant: `alice.db` is at every instant either the complete old database or the complete restored one, and any failure before the rename leaves it untouched. An older backup is migrated at that start with its own pre-migration backup. `secrets.toml`, `config.toml`, the token and the spool are never touched; spooled writes are applied through the normal redaction path once the restored database is served. A restore does not undo three things: memories written after the backup leave the active database (they stay in the before-restore copy), memories purged after the backup come back, and text stored under older redaction rules is not scanned again.

### 5.7 Error handling

| Failure | Behaviour |
|---|---|
| Daemon unreachable | Writes go to the spool; reads return a short error telling the agent memory is starting and to retry in a few seconds (with what it is doing, e.g. downloading the model). |
| Daemon cannot start (model download or load fails, database damaged or from a newer schema, failed migration) | The daemon writes the reason to `daemon.error` and exits; every client reports that reason and the next step at once (`memory daemon could not start: <reason>`), writes go to the spool with that reason, and `alice status` shows it. A start that succeeds clears it. *As built (M2):* `alice doctor` reports what it can see with the next step; serving a damaged database read-only is not implemented. |
| Embedding provider fails | Memory stays lexically searchable; `embed_state = retry` with exponential backoff; health shows the backlog. |
| Judge fails / over budget | Heuristic fallback; `judge_state = retry`. |
| Vector index missing or inconsistent | Rebuild on a background thread; lexical search only meanwhile (`health.index_ready = false`). |
| Disk full, DB corruption while running | Read-only mode. Writes go to the spool, and the reply says why. Agents receive: `memory is read-only: <reason>. See "alice status" and the daemon log ("alice daemon logs") for the cause.` (*As built (M2):* the text still points at `alice status` and the log; `alice doctor` shows the read-only reason with what to do next.) |
| Client timeouts | `recall` 5 s, `context` 3 s; writes acknowledge immediately. No 120 s waits. |

All MCP and CLI errors state what happened and what to do next, in one or two sentences.

---

## 6. Settings

- **One schema** defined as Rust structs (`serde` + `schemars`). The JSON Schema is served at `GET /api/v1/settings/schema`; the UI form (SP3), `alice config` and the reference docs are generated from it.
- **Layers** (later wins): built-in defaults → `~/.alice/config.toml` → `<repo>/.alice.toml` (project scope) → environment variables (`ALICE_<SECTION>_<KEY>`). *As built (M2):*
  - The variable name is the dotted key upper-cased with dots as underscores: `judge.review_threshold` is `ALICE_JUDGE_REVIEW_THRESHOLD`, `judge.jev.rate_per_sec` is `ALICE_JUDGE_JEV_RATE_PER_SEC`. The environment wins over the files, and over stored secrets (`TYPESAFE_API_KEY` and `ALICE_EMBEDDING_API_KEY` are shown with source `env`). An `ALICE_*` variable that is not a setting and not one of the known others (`ALICE_HOME`, `ALICE_LOG`, `ALICE_AGENT`, `ALICE_NO_AUTOSTART`, `ALICE_MODELS_DIR`, `ALICE_EMBEDDER`, the test and evaluation variables) is ignored with a warning, shown in health, `alice config list`, `/api/v1/settings` and `alice doctor`; it never sends the other settings back to their defaults. A variable with a bad value is reported and the previous value stays.
  - **Project scope is limited to `[privacy]` and can only tighten it**: `block_secrets`, `custom_patterns` and `cloud_denied`. Any other key in `.alice.toml` is refused. An invalid, unreadable or blank project file fails closed (secrets blocked, the last good patterns kept, the project kept from the cloud), and a loosening of it waits for three spaced looks.
- **Live reload:** changes through the API or edits to the files apply without restarting the daemon. Invalid values are rejected with the schema error; the previous value stays in effect. *As built (M2):*
  - `config.toml` is polled every 2 s (no file-watcher crate) and read once it has held still for 30 ms, so an editor that truncates and then writes is not caught half way. An invalid file or value never replaces valid settings (a leaf that is invalid costs only itself; privacy keys are salvaged); the errors are shown by `alice config list`, `/api/v1/settings` and `alice doctor`.
  - **Fail-closed change control (the loosening ratchet).** A change that would loosen a protection applies only when the same content is read on three polls in a row (about 6 s); a change that tightens applies at once. The keys that count as loosening (`loosened_protections`): `privacy.redact` off, `privacy.redact_mode` from `block` to `redact`, an entry removed from `privacy.cloud_denied_projects` or `privacy.custom_patterns`, an entry added to `privacy.allowlist`, a higher `judge.daily_budget_usd`, `judge.provider` switched to `jev`, `embedding.provider` or `embedding.endpoint` switched to a destination that is not on this machine (or to another remote host), and `mcp.admin_tools` switched on; emptying or removing a non-empty `config.toml` counts too. A change through the API or `alice config set` is a deliberate, complete write: it applies at once even when it loosens, but the reply says so and the `settings.change` event names the loosened keys.
  - **`config_degraded`.** While no valid `config.toml` has ever loaded in this process, the daemon runs on defaults and `config_degraded` is true; every cloud consumer (the Jev judge, HTTP embedding providers) then treats it as "no cloud", because a typo inside `privacy.cloud_denied_projects` itself would otherwise mean an empty denylist. Once a valid file has loaded it stays false: a later invalid edit is not applied at all.
  - **The one cloud gate** is `SettingsHandle::cloud_allowed(project, root)`, used by the judge and the embedding providers once per call that would leave the machine (and before each send attempt): it answers no when the configuration is degraded, the slug is empty, the global denylist names the project, the project's `.alice.toml` sets `cloud_denied`, or that file is broken. It asks for every identity of a project (slug, aliases, display name, the name as asked, slugified), and names are compared slugified on both sides, so an entry written `Client X` denies `client-x`.
  - **Restart-only keys** (accepted, saved, applied at the next daemon start; the reply says so): `server.port`, `secrets.backend`, `embedding.idle_unload`, `embedding.batch_size`. `storage.data_dir` is read-only (set `ALICE_HOME`). A change of `embedding.provider`, `.model` or `.endpoint` needs no restart: it starts a background re-embed (§4.2).
- **Secrets** (Jev key, embedding API keys, git credentials if not using the system helper) are stored by default in `~/.alice/secrets.toml` with mode 0600. The OS keychain via `keyring` (macOS Keychain, Windows Credential Manager, Secret Service on Linux) is opt-in (`secrets.backend = "keychain"`): on macOS every rebuilt or updated binary is a new identity to the Keychain and blocks on a GUI permission prompt, which a background daemon cannot answer, and on Linux the Secret Service needs a D-Bus session that headless machines often lack. The API never returns secret values, only `{set: true, hint: "••••ab12"}`. *As built (M2):* the secret names are `jev_api_key` and `embedding_api_key`; `secrets.toml` is written under an exclusive lock (`secrets.toml.lock`) and a file others can read is made private again; the environment variables `TYPESAFE_API_KEY` and `ALICE_EMBEDDING_API_KEY` override stored values and are never read into settings or logged. A hand-edited `secrets.toml` is read when the judge is next rebuilt (a `judge.*` change, or a key set or deleted through the API), so after editing it by hand run `alice daemon stop` and start the daemon again. Switching `secrets.backend` does not migrate stored secrets: set them again under the new backend. Secret changes are recorded as events (that a secret was set, never its value).
- **API:** `GET /api/v1/settings`, `PATCH /api/v1/settings`, `GET /api/v1/settings/schema`, `GET /api/v1/settings/flags` (the few switches other processes read often, `mcp.admin_tools`), `PUT /api/v1/secrets/{name}`, `DELETE /api/v1/secrets/{name}`, `POST /api/v1/settings/test/{provider}` (connection test: `judge` or `embedding`).
- **CLI:** `alice config list|get|set|unset|edit`, `alice config set-secret <name>` (reads from stdin, never from argv), `alice config delete-secret <name>` and `alice config test <provider>`.

Groups and main keys:

```toml
[storage]       data_dir (read-only), raw_retention
[embedding]     provider, model, endpoint, batch_size, idle_unload
[search]        default_max_tokens, relevance_floor, relevance_sure, relevance_margin,
                raw_fallback_min_results, [search.weights] …, [search.half_life] …
[capture]       per-agent enable flags (used by SP2)
[judge]         provider, auto_apply_threshold, review_threshold, daily_budget_usd, timeout_ms,
                cluster_similarity
[judge.jev]     model, endpoint, rate_per_sec
[distill]       provider, model (used by SP2)
[privacy]       redact, redact_mode, custom_patterns, allowlist, cloud_denied_projects
[sync]          remote, branch, auto, include_raw, excluded_projects   (M4)
[backup]        daily, weekly, dir
[compat]        mempalace   (M3)
[server]        port (0 = random), open_browser
[mcp]           admin_tools
[secrets]       backend (file | keychain)
[maintenance]   enabled, idle_secs, trash_days, events_days
```

---

## 7. Git sync

Git is the transport. The payload is readable text, never the database file.

### 7.1 Repository format

```
<repo>/
  .alice-sync.json                                  format version, created_by
  <project>/<kind>/<yyyy>/<ulid>-<slug>.md          one memory per file
  kg/entities.jsonl                                 sorted by id
  kg/relations.jsonl                                sorted by id
  kg/predicates.jsonl                               sorted by name
  projects.jsonl                                    projects + aliases, sorted
  raw/<project>/<source-id>.jsonl                   only if sync.include_raw = true
```

Memory file:

```markdown
---
id: 01J9Z6R4T8…
project: pg-ai
kind: decision
title: v4 frames must be framed protobuf
topics: [v4, protocol]
importance: 4
status: active
author: {agent: claude-code, model: claude-opus-5-5, session: 7e78e423}
created_at: 2026-10-12T07:09:03Z
updated_at: 2026-10-12T07:09:03Z
hlc: 2026-10-12T07:09:03.000Z-0001-mbp1
supersedes: [01J8…]
links: [{to: 01J7…, type: relates_to}]
---
Body text in Markdown.
```

Serialization is deterministic (fixed key order, sorted lists, LF endings) so unchanged memories never produce diffs. The repository opens cleanly in GitHub and Obsidian.

Embeddings, events, review items, caches and costs are local-only and never synced.

### 7.2 Sync algorithm

Uses the system `git` binary, so the user's SSH keys and credential helpers work unchanged. The sync working copy lives at `~/.alice/sync/`.

1. **Export:** write files for memories, KG and projects changed since the last synced HLC; delete files of trashed memories; commit (`alice sync: <device> <n> changes`).
2. **Fetch** the remote branch.
3. **Merge per record** (Alice merges, git never writes conflict markers): compute changed and deleted paths between the last synced base commit and the fetched head.
   - Changed only remotely → import.
   - Changed on both sides → higher HLC wins (HLC = hybrid logical clock with device id tiebreak); the losing version is stored as a `sync_conflict` review item.
   - Deleted remotely, unchanged locally → move to trash locally.
   - Deleted remotely, changed locally → keep local and create a review item.
   - KG JSONL files merge by record id with the same rule.
4. **Write** the merged state, commit the merge, **push**.
5. If the push is rejected because the remote moved, repeat from step 2 up to 3 times, then report the failure in health/UI. Local data is never modified by a failed sync.

### 7.3 Controls

- UI Sync page (SP3): remote, branch, **Push**, **Pull**, **Sync now**, ahead/behind counts, last sync time, pending conflicts.
- Auto-sync: `off`, `idle`, or an interval such as `15m`.
- CLI: `alice sync init <remote>`, `alice sync push`, `alice sync pull`, `alice sync status`.
- Per-project exclusion via `sync.excluded_projects` or `.alice.toml`.
- First-time setup warns that memories may contain sensitive information and recommends a private repository. Redaction applies to exports.

---

## 8. Interfaces

One service layer, three adapters (MCP, CLI, HTTP), so behaviour is identical everywhere and tested once.

### 8.1 MCP tools (core set)

Names are verbs without the brand, so a rename never breaks prompts.

| Tool | Parameters | Returns |
|---|---|---|
| `recall` | `query`, `project?`, `topics?`, `kinds?`, `since?`, `before?`, `include_raw?`, `mode? (fast\|precise)`, `max_tokens?`, `format? (text\|json)` | Ranked results, `related`, omitted count |
| `remember` | `content`, `kind?`, `title?`, `project?`, `topics?`, `importance?`, `supersedes?`, `idempotency_key?` | `{id, status: new\|duplicate\|pending}` |
| `context` | `project?`, `max_tokens?` (default 1,200) | Briefing: high-importance and pinned memories, recent decisions, active warnings, latest handoff, user preferences, pending review count |
| `get_memory` | `ids[]`, `include_provenance?` | Full memories with links and provenance |
| `update_memory` | `id`, `action (edit\|supersede\|archive\|forget\|restore)`, fields | Updated memory and event id |
| `query_facts` | `entity`, `predicate?`, `as_of?`, `timeline?` | Relations valid at the time, or a timeline |
| `assert_fact` | `subject`, `predicate`, `object`, `valid_from?`, `action (add\|supersede\|end)` | Relation id; predicate suggestion if a similar one exists |

**Admin toolset** (disabled by default; `mcp.admin_tools = true`): `review_list`, `review_resolve`, `merge_memories`, `stats`. *As built (M2):* the stdio shim asks the daemon (`GET /api/v1/settings/flags`, cached for 1 s, fail closed) before every tool listing and call, so there is one authority for the switch; with the daemon down the admin tools are simply not offered. Turning the switch on is a loosening of a protection (§6).

**Other MCP features**

- `instructions` in the initialize result (≈ 150 tokens): call `context` at session start if the host did not inject it; `recall` before answering about past work; `remember` decisions, fixes and lessons; write a `handoff` before ending long tasks. This replaces protocol blocks pasted into CLAUDE.md.
- **Project resolution:** an explicit `project` argument wins; otherwise the client's first workspace root (`roots/list`, asked once per session and again after `roots/list_changed`) decides; otherwise the stdio shim's working directory; otherwise `global`. Clients on the handshake-less protocol cannot be asked for roots and fall through to the shim's directory.
- **Author:** the MCP client's name (`clientInfo.name`, e.g. `claude-code`) is recorded as `author.agent` on every memory the client writes.
- Prompts: `recall` and `remember-session` (Claude Code exposes them as slash commands).
- Resources: `memory://context` (current project briefing) and `memory://m/{id}`.

### 8.2 MemPalace compatibility layer

Opt-in (`compat.mempalace = true`; enabled automatically after `alice import mempalace`, can be turned off).

| MemPalace tool | Alice behaviour |
|---|---|
| `mempalace_status` | `context` output (compact), plus wing→project map summary |
| `mempalace_search` | `recall`; `wing` → project, `room` → topic |
| `mempalace_add_drawer` | `remember`; `wing` → project, `room` → topic |
| `mempalace_diary_write` / `diary_read` | `remember(kind=diary)` / recent diaries via `recall` filter |
| `mempalace_get_drawer`, `update_drawer`, `delete_drawer` | `get_memory` / `update_memory` (resolves `legacy_id`) |
| `mempalace_kg_query`, `kg_add`, `kg_invalidate`, `kg_supersede`, `kg_timeline` | `query_facts` / `assert_fact` |
| `mempalace_list_wings`, `list_rooms` | Project and topic lists |

Other MemPalace tools are not provided; calling them returns a message naming the Alice equivalent.

### 8.3 CLI

```
alice recall <query> [--project p] [--kind k] [--topic t] [--raw] [--precise] [--limit n]
             [--max-tokens n] [--since d] [--before d]
alice remember [--kind k] [--title t] [--topic t]... [--project p] [--importance 1-5]
               [--supersedes id]... (<text> | -)
alice context [--project p] [--max-tokens n]
alice get <id>... [--provenance]       alice update <id> --action edit|supersede|archive|forget|restore ...
alice facts <entity> [--predicate p] [--as-of date] [--timeline]
alice fact add|supersede|end <subject> <predicate> <object> [--from date]
alice history [target] [--actor a] [--limit n]       alice undo <event-id>
alice review [list [--type t] [--all] | show <id> | resolve <id> <choice>]
alice stats                 alice projects              alice status
alice backup now|list       alice restore [backup] [--yes]
alice maintenance status | run <job>
alice config list|get|set|unset|edit|set-secret|delete-secret|test
alice doctor                alice bench                 alice model fetch
alice daemon [start|stop|status|logs|run]
alice service [install|uninstall|status] [--dry-run] [--target-dir d]
alice token rotate [--show]
alice mcp
(planned: alice sync … (M4), alice import mempalace … (M3), alice ui (SP3), alice hook <event> (SP2))
```

Human-readable tables by default, `--json` for scripts (every command; `alice restore` and `alice doctor` print exactly one JSON document). The CLI supports every write operation, so agents without MCP access can use Alice through shell commands.

*As built (M2):*
- **`alice status` and `GET /api/v1/health`** report the daemon, counts, the embedding and judge backlogs, open reviews (`reviews_open`), whether the model is loaded (`model_loaded`), writes waiting in the spool (`spool_pending`), the newest backup (`last_backup`), the memory in use (`memory_bytes`: physical footprint on macOS, resident set on Linux, absent on Windows), `settings_errors` (a list of `{key, message}`), ignored `ALICE_*` variables and `config_degraded`.
- **`alice doctor`** (also `GET /api/v1/doctor`, `--json`) first makes the checks that need no daemon — `config.toml` and the `ALICE_*` variables, the permissions of the data directory, the token and `secrets.toml`, the embedding model on disk, whether a daemon or a service unit exists — and, when a daemon answers, asks it for the rest: the database (read-only after a full disk), maintenance jobs and backups (a job counts as failing only while its last error is newer than its last success), the vector index, embeddings that failed or that the cloud policy holds back, a re-embed in progress, the judge (a missing Jev key, the day's budget), open reviews, the spool, settings that were refused or wait for a restart, memory and free disk. It never starts a daemon and never reads a secret (keys are reported as set or not); every line that is not fine says what to do next; the exit status is 1 when a check fails. With `ALICE_EMBEDDER=fake` the embedding-model check warns instead of failing.
- **`alice token rotate [--show]`** replaces the token (§2.2 rule 6); with no daemon it works under the daemon lock, so no daemon can start on the old token meanwhile. The token is printed only with `--show`.
- **`alice service install`** writes the launchd plist (`~/Library/LaunchAgents/dev.alice.daemon.plist`), the systemd user unit (`~/.config/systemd/user/alice.service`, which needs `loginctl enable-linger` to outlive a logout) or registers the scheduled task; the unit runs `alice daemon run` with the data directory the command was run with and holds no secret. `--dry-run` prints files and commands and changes nothing; `--target-dir d` writes files under `d` and runs no command. `alice daemon stop` ends the daemon cleanly, so a unit does not restart it until the next login. Tests use only `--dry-run`, `--target-dir` and a recording stand-in for the service manager.
- Additions beyond the list above: `alice history`, `alice stats`, `alice backup`, `alice maintenance`, `alice config unset|delete-secret|test`, `alice review show`, and the `ALICE_NO_AUTOSTART` variable (commands never start a daemon; writes are spooled), which tests and scripts use.

### 8.4 HTTP API

`/api/v1/` JSON endpoints for memories, search, context, facts, projects, topics, graph (nodes/edges with filters and paging), reviews, events/undo, settings/secrets, sync, health and stats. OpenAPI generated from the handlers. `GET /api/v1/events/stream` (SSE) pushes changes, review items, sync status and health to the UI.

*As built (M2)* — every route needs the bearer token and passes the `Host`/`Origin` checks of §2.2, except `GET /ping`, which is public (Host and Origin only) because it proves the daemon's identity before a client sends the token: `GET /ping`, `GET /health`, `GET /doctor`, `GET /projects`; `POST /remember`, `/recall`, `/context`, `/get`, `/update`, `/ingest`, `/facts/query`, `/facts/assert`; `GET /events` and `POST /undo`; `GET /reviews`, `GET /reviews/{id}`, `POST /reviews/{id}/resolve`; `POST /memories/merge`; `GET /stats`; `GET|POST /backups`; `GET /maintenance`, `POST /maintenance/run`; `GET /events/stream` (SSE: events, review items, settings changes and health changes, which are checked every 5 s); `GET|PATCH /settings`, `GET /settings/schema`, `GET /settings/flags`, `POST /settings/test/{provider}`, `PUT|DELETE /secrets/{name}`; `POST /admin/shutdown` and `POST /admin/token/rotate`. `POST /admin/token/rotate` replaces the token file atomically (0600) and the running daemon switches at once, refusing the old token from the next request on. Restore is not an endpoint (it is CLI-only, §5.6). OpenAPI generation and the graph and topics endpoints wait for SP3, the milestone that consumes them; the settings API, its schema and the SSE stream are the UI contract delivered now.

### 8.5 Contracts for SP2 and SP3

- **SP2 hooks:** `alice hook <event>` reads the host's hook JSON on stdin, sends it to the daemon (or the spool) and prints host-specific output (for SessionStart, the `context` briefing as additional context). Must finish in < 100 ms. Transcript ingestion uses `POST /api/v1/ingest` with chunk batches (at most 1,000 chunks per request); distillation results use `remember` with provenance.
- **SP3 UI:** consumes the HTTP API, SSE stream and settings schema only; the daemon serves the built UI assets from `/`.

---

## 9. MemPalace importer

`alice import mempalace [--palace <path>] [--dry-run] [--since <date>]`

- Reads the palace directly; MemPalace does not need to be installed or running. Supports the `sqlite_exact` and `chroma` backends and `knowledge_graph.sqlite3`.
- Copies the palace databases to a temporary snapshot first (SQLite backup API, read-only), so a running MemPalace hub is never locked.
- **Mapping**
  - Drawers whose source is a transcript (`*.jsonl`, or wing `sessions`) → raw tier: one `source` per transcript path, drawers become ordered chunks.
  - Drawers mined from project files (code/docs chunks) → raw tier, `source.type = file`.
  - Diary drawers and drawers written by agents (`added_by` set, no source file) → knowledge tier. Kind from room name: `decisions` → decision, `bug-classes`/`issues` → bugfix, `methods`/`runbook` → howto, `status`/`progress` → status, `diary` → diary, `knowledge` → fact, otherwise note; refined by the Judge if enabled.
  - Wings → projects through normalization and the alias table. Agent wings (`wing_<agent>`, `wing_task_*`, `wing_impl*`, `wing_fix*`) → the project is inferred from content (project names and paths mentioned), else `global`; the wing suffix becomes `author.agent`.
  - Rooms → topics. Closets are dropped (Alice rebuilds its own indexes).
  - Exact duplicates are collapsed (earliest kept); every original id is stored in `legacy_id` for compat lookups.
  - KG entities and triples → entities and relations with validity periods. Similar predicates produce review items rather than automatic merges.
  - Tunnels → `relates_to` links between the corresponding projects' memories where resolvable, otherwise dropped and reported.
  - AAAK text is kept verbatim.
- Embeddings are recomputed in the background with Alice's model; lexical search works immediately.
- Resumable (per-batch checkpoints) and incremental (`--since`, or automatic: skips `legacy_id`s already imported), so Alice can run alongside MemPalace during migration.
- Final report: counts per tier and project, the wing → project map, duplicates collapsed, items queued for review, anything skipped and why.

---

## 10. Testing and benchmarks

- **Unit tests** per module: store, migrations, normalization, redaction, ranking, Judge providers (Jev via recorded fixtures), HLC, sync merge rules, importer mapping.
- **Concurrency regression (the MemPalace failure):** 20 clients writing and searching concurrently while an import of 100k records runs. Pass: `remember` p95 < 50 ms, `recall` p95 < 200 ms, zero lost or duplicated writes. The test (`crates/alice-daemon/tests/service_concurrency.rs`) ingests 20k chunks by default, which CI runs; `ALICE_TEST_BULK=100000` runs the full 100k (best with `--release`, where the latency budgets are enforced).
- **Crash safety:** `kill -9` during writes, during import and during sync; on restart no acknowledged write is lost (spool + WAL); a deleted or corrupted vector index rebuilds automatically. M1 tests: SIGKILL while three bulk ingests and forty `remember`s are in flight, then every acknowledged id is present, every queued write applied and the resent ingests complete (`crates/alice/tests/cli.rs`); a deleted, truncated or garbage-filled index is rebuilt at start (`service.rs` tests). Sync arrives in M4.
- **Sync:** two simulated devices and a bare remote: concurrent edits, deletes on one side, edit-vs-delete, push races, KG merges; verify deterministic files and review items.
- **Search quality:** gold query sets scored with nDCG@10 and Recall@10. A synthetic multilingual fixture (Thai, English, mixed, code identifiers) lives in the repo and runs in CI. A gold set built from the owner's palace runs locally only and is never committed. The Jev Judge can act as relevance grader when enabled.
- **Interfaces:** MCP conformance with a reference client, CLI golden tests (snapshot), HTTP auth tests (missing token, wrong Host, foreign Origin).
- **Security:** redaction corpus covering every built-in detector; log scanning for secrets in tests.
- **Benchmarks:** `alice bench` and Criterion suites for write acknowledgement, search at 100k and 1M synthetic chunks, import throughput and RSS. Results recorded per release.

CI: GitHub Actions on macOS, Linux and Windows (fmt, clippy `-D warnings`, tests, synthetic search eval).

---

## 11. Distribution

- `cargo-dist` builds release binaries with checksums for the targets ONNX Runtime ships prebuilt libraries for: macOS arm64, Linux glibc x64 and arm64, Windows x64. There are no musl or Intel macOS builds with local embeddings (no prebuilt ONNX Runtime); those targets ship without the local model and run with an HTTP embedding provider (built in M2, §4.2). No release has been made yet.
- *As built (M2), Windows:* Windows x64 builds, and CI runs `fmt`, `clippy` and the in-process test suite there; tests that start processes (detached daemon start, `daemon stop`, the CLI process tests) run on Unix only. The real model is verified on Windows by the manually started `windows-real-model` workflow, with the data directory under a non-ASCII path. Real-machine verification of the Windows daemon start/stop and `alice service install` (a logon scheduled task) is pending: they are covered by unit and plan tests only.
- Install paths: Homebrew tap (`brew install icueth/tap/alice`), shell/PowerShell installer scripts, npm wrapper (`npm i -g alice-memory`), `cargo install alice-memory`.
- Binary name `alice`; package name `alice-memory` on crates.io and npm (`alice` is taken on both).
- `alice self-update`; update checks are opt-in.
- License MIT. Credits MemPalace in the README and in any file that adapts its code.

---

## 12. Milestones and acceptance criteria (SP1)

| Milestone | Delivers | Accepted when |
|---|---|---|
| **M1 — Store, search, interfaces** | Workspace, schema + migrations, daemon (auto-start, lock, token, spool), write path steps 1–4 + embedding/indexing, hybrid search with Thai/code support, `context`, 7 MCP tools, CLI, HTTP API skeleton | Concurrency regression and crash-safety tests pass; performance budgets met at 100k synthetic; Claude Code and Codex can use Alice over MCP |
| **M2 — Judge, maintenance, settings** | Judge layer (heuristic + Jev), relation-to-existing, review inbox, events/undo, redaction, scheduler jobs, backups/restore, settings schema/API/live reload/keychain, health | Jev fixtures and live smoke test pass; budget stop verified; settings changes apply live; restore round-trip verified. *Built:* also the 800 MB memory budget (§4.6), HTTP embedding providers (§4.2), `alice doctor` and `alice service install`; results in `docs/benchmarks/m2.md`, `m2-jev.md` and `m2-embedding.md` |
| **M3 — MemPalace import + compat** | Importer, compat layer, import report | The owner's palace imports with zero errors; gold-set search quality beats MemPalace; existing `mempalace_*` prompts work against Alice |
| **M4 — Git sync** | Export format, merge engine, CLI, auto-sync | Two-device sync tests pass; the owner syncs two machines through a private repo |

---

## 13. Risks and mitigations

| Risk | Mitigation |
|---|---|
| ONNX Runtime packaging differs per platform | Use `fastembed`/`ort` download-or-bundle strategies; CI builds and smoke-tests embeddings on all three OSes; Ollama/OpenAI providers as fallback |
| Thai search quality with trigram + e5-small | Gold-set evaluation in CI; precise mode via Judge; reranker hook kept in the pipeline |
| Judge mistakes corrupt memory | Thresholds, review inbox, supersede instead of delete, full event history and undo |
| Jev availability, pricing or model changes | Optional by design; pinned model; cache; budget stop; heuristic fallback |
| Sensitive data in public sync repos | Redaction on export, project exclusions, first-run warning, encryption planned post-v1 |
| Scope of SP1 is large | Four milestones, each independently useful and shippable |

---

## 14. Decisions log

| Decision | Choice | Reason |
|---|---|---|
| Language | Rust | Single static binary removes Python environment drift; low memory with many concurrent clients; shared crates with the Tauri desktop app in SP3 |
| Storage | SQLite (WAL) + FTS5 + usearch HNSW | Embedded, robust, one file to back up; the index is disposable and rebuildable |
| Embedding default | multilingual-e5-small, local ONNX | Thai + English quality without API keys |
| Concurrency | Daemon as sole DB owner, heavy work off the writer | Fixes the root cause of MemPalace write timeouts |
| Memory model | Knowledge vs raw tiers; agent as metadata | Fixes noise and wing sprawl |
| Judge | Provider-neutral, heuristic default, Jev optional | Calibrated probabilities enable safe automation; works offline without it |
| Sync | Git transport, one Markdown file per memory, record-level merge | Readable, diffable, multi-machine, no binary DB in git |
| MCP surface | 7 core tools + opt-in admin set + compat layer | Lower token cost and less tool confusion than 45 tools |
| Name | Alice (Agent Long-term Intelligent Context Engine); packages `alice-memory` | `alice` taken on crates.io and npm; GitHub and Homebrew names free |
| License | MIT | Matches MemPalace; widely accepted by developers |
| Default embedding build (M1 spike) | int8 `multilingual-e5-small-q8` from `Xenova/multilingual-e5-small`, pinned revision, 118 MB model + 17 MB tokenizer, SHA-256 verified | A quarter of the f32 model's size (470 MB); fastembed batches it with `QuantizationMode::Static` |
| Secrets storage (M1 spike) | 0600 file by default; OS keychain opt-in | `keyring` blocks on a macOS permission prompt after every binary rebuild and needs D-Bus on Linux; a daemon must never block on a prompt |
| Release targets (M1 spike) | Follow the ONNX Runtime prebuilt matrix: macOS arm64, Linux glibc x64/arm64, Windows x64 | `ort` has no prebuilt library for musl or Intel macOS; local embeddings there wait for HTTP providers |
| Vector index layout (M1) | One usearch index per tier (memories, chunks) per model | Memory searches never walk the much larger chunk graph or wait for chunk ingest |
| Daemon discovery (M1) | Challenge-response ping (HMAC over a client nonce and the instance id) before any token is sent | A stale `daemon.json` must not hand the token to whatever process now owns the port |
| Embedder instances (M1 benchmarks) | One local model shared by the worker and queries, token-bounded batches, fair hand-over | A second instance cost ~600 MB; a query waits for one short batch at most |
| SQLite memory (M1 benchmarks) | No `mmap`; at most 8 readers with 16 MiB caches | 64 concurrent recalls opened 64 connections, each mapping 256 MB: the daemon reached 3.3 GB RSS at 1M chunks |
| Raw-tier ranking (M1 benchmarks and review) | Every term with a match reads its newest max(k, 5,000 / terms) matches and every row read is summed; `_` identifiers searched as one token in the identifier list, which holds the joined form of every name; trigrams: non-ASCII terms with a match read the same share, ASCII terms their newest k; ties taken term by term; in the raw tier every word of a script written without spaces is searched by the trigram list only; every list splits queries at ASCII and Unicode punctuation; joined identifier forms searched by the identifier list only | Ranking every match of a common word cost 300–400 ms per list at 1M chunks (`include_raw` p95 947 ms). A window shared by all terms let a common term hide a rarer one (review finding I1); the controller ruled that every term gets its own share and that matches beyond it are taken newest-first. Keeping only each term's own top k before summing lost chunks holding every query word (review N1); dropping `_` identifiers from the word list needed every written form indexed with the joined token (review N2, controller ruling). Leaving only Thai words with combining marks to the trigram list split a rare marked word from a common unmarked one across two lists, and trigram pieces split only at whitespace lost marked words touching punctuation (fix round 4, controller rulings R1–R5) |
| Chunk vectors (M2) | A flat sign-bit index (one bit per dimension above the mean), a Hamming shortlist of `max(4,000, 32·√N)` re-ranked in RAM against the float query, then `max(k, 400)` rows rescored by exact cosine from the stored f16 vectors | The int8 HNSW over 1M chunks held 508 MB resident and was the largest part of the memory budget; the sign-bit index is 53 MB and keeps recall@10 ≥ 0.97 at 96,000 real passages (gate), at ~9 ms of CPU per raw query. A binary HNSW, a memory-mapped index (its touched pages count towards RSS, the problem M1 fixed for SQLite) or dropping chunk vectors were rejected. At ~10M chunks a graph would be needed again |
| Local embedder (M2) | ONNX Runtime used directly (`ort`) with a compact Unigram tokenizer in `alice-embed`; fastembed only as a dev-dependency for the equivalence test | The `tokenizers` trie cost ~250 MB and fastembed's session kept its largest call's memory; one instance fell from ~600 MB to ~348 MB with identical vectors (0 token mismatches, cosine 1.000000). Literal special-token text in a passage is tokenised as text, a documented difference |
| Idle unload (M2) | `embedding.idle_unload` default 10 minutes (`0` = never): the session is dropped; a query that arrives meanwhile uses word matches while the model reloads in the background | Gives back ~190 MB of an idle daemon at the cost that the first recall after a quiet period can miss a vector-only match |
| usearch build (M2) | Built without NumKong on every OS | NumKong's kernels do not link under MSVC (`LNK2019`), and one code path everywhere is simpler to test; memory-vector distance is computed by usearch's portable code, a few milliseconds slower at large memory counts |
| Memory activation (M2) | A memory becomes `active` after the judge stage; `pending` and `active` are one state for search, context and undo | The spec's own §5.1 order; a pending memory is findable at once, so nothing is hidden while a slow judge works |
| Jev question shape (M2) | Options shuffled with a seed derived from the request; the reversed question asks the converse relation (`outdated_by`) of the same pair in the same request | Identical decisions build identical bodies and hit the SHA-256 cache; the order bias of one question is cancelled by the pair |
| Jev budget (M2) | A call reserves an upper bound of its cost before it is made (body bytes × the highest tokens-per-byte ratio seen, starting at 1.5, + 300 tokens) and settles to the reported usage; a spend that cannot be read counts as an exhausted budget | The day's spend never passes `judge.daily_budget_usd`, whatever Jev reports; a transient read error can only pause Jev, never allow a second budget. Cost of being wrong: the day's last call can be refused while room remains |
| Jev failures (M2) | Retries only on 429, 5xx and timeouts; a connection error is not retried: the heuristic decides, the memory is marked `retry` and judged again after a back-off | No time or budget is spent hammering a host that cannot be reached; the hourly/5-minute retry asks again |
| Judge candidates (M2) | The 5 nearest memories by vector plus up to 3 more of the same kind whose titles are nearly the same | An update worded differently from its predecessor falls outside the vector top 5; the title matches catch it at the cost of up to 3 more comparisons per memory |
| Heuristic judge (M2) | Auto-applies only a duplicate with the same text after NFC and whitespace collapse (never on cosine) and an identical-stem predicate; everything else is review-only | Silently losing the newer fact to a similarity guess is worse than a review item; offline installs get fewer automatic merges and more review items |
| Entity proposals (M2) | Deferred to SP2 | Jev answers choice questions only, so no M2 provider can name a new entity; mentions link known entities |
| FTS maintenance (M2) | Bounded `merge` steps (500 pages per writer slice) instead of a weekly `optimize` | `optimize` rewrites the whole index in one write transaction and would hold the single writer for seconds to minutes at 1M chunks |
| Review type and actor (M2) | Review type `undo` and actor `system` added | An undo conflict needs a question; maintenance needs an actor |
| Project scope (M2) | A project's `.alice.toml` may only tighten privacy (`block_secrets`, `custom_patterns`, `cloud_denied`) | A repository file must not be able to weaken what the owner configured; per-project tuning of search or judge would need a later extension |
| Settings robustness (M2) | An unknown `ALICE_*` variable is warned about, not fatal; loosening a protective key in `config.toml` needs three identical polls; the environment wins over files | A typo must not send every other setting back to defaults, and a half-written file must not switch redaction off for a moment |
| Restore (M2) | Runs in the CLI under the daemon lock, never through a daemon endpoint; the invariant is that `alice.db` is always complete, old or restored | No daemon can run while files are swapped; writes made meanwhile are spooled and applied after |
| Undo (M2) | Reverts a whole event group; purged memories cannot come back; events are pruned after 365 days | One user action is one undo; the undo window is one year |
