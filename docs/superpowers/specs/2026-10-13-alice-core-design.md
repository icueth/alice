# Alice Core — Design Spec (Sub-project 1)

- **Date:** 2026-10-13
- **Status:** Draft, awaiting owner review
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
4. **Hooks never block and never lose data.** If the daemon is unreachable, the client appends the request to `~/.alice/spool/<pid>-<ts>.jsonl` (fsync), triggers a detached daemon start, and returns. The daemon drains the spool on start-up and on a 5 s timer.
5. **Auto-start.** The first client that cannot reach the daemon starts it detached. A lock file (`~/.alice/daemon.lock`, OS advisory lock) guarantees a single instance. `alice service install` registers a launchd agent (macOS), systemd user unit (Linux) or scheduled task (Windows) for always-on operation.
6. **Local security.** The daemon binds to 127.0.0.1 on a random free port, written with the pid and a per-start instance id to `~/.alice/daemon.json`. Every request carries a bearer token read from `~/.alice/token` (mode 0600, regenerated on `alice token rotate`). Before sending the token, a client proves it is talking to that daemon: `GET /api/v1/ping?nonce=<random>` must answer with the instance id from `daemon.json` and `HMAC-SHA256(token, "<nonce>:<instance>")`, so a stale `daemon.json` pointing at a port another process now uses never receives the token, and a proof relayed from another daemon does not vouch for a stale instance. The server rejects a request if its `Host` is not `127.0.0.1:<port>` or `localhost:<port>`, or if it carries an `Origin` other than the daemon's own origin (prevents DNS rebinding and cross-site requests). The UI gets a session through a one-time URL printed or opened by `alice ui`.
7. **Two MCP transports.** Clients that support Streamable HTTP connect to `/mcp` directly (with the token header). Others launch `alice mcp`, a stdio shim that proxies to the daemon (auto-starting it); target shim start-up < 20 ms.

### 2.3 Filesystem layout

`ALICE_HOME` (default `~/.alice` on all OSes):

```
alice.db            SQLite database (source of truth)
vectors/            HNSW index, two files per embedding model: <model>.memories.usearch and
                    <model>.chunks.usearch (rebuildable)
models/             downloaded embedding models
spool/              pending requests written while the daemon was down
backups/            rotated SQLite snapshots
logs/               daemon logs (rotated, never contain secrets)
config.toml         user configuration
daemon.json         {pid, port, instance, version, started_at}
daemon.lock
token               API token (0600)
```

### 2.4 Rust workspace

```
crates/
  alice-core/     domain model, store (SQLite), search, judge, redact, sync, import, settings schema
  alice-embed/    embedding providers (ONNX local, Ollama, OpenAI-compatible, Voyage)
  alice-daemon/   HTTP server, MCP server, workers, scheduler, service layer
  alice/          the `alice` binary: CLI, MCP stdio shim, hook handler, daemon entry
```

Key dependencies (versions pinned in the implementation plan): `rusqlite` (bundled, FTS5), `usearch`, `fastembed` / `ort`, `rmcp` (official MCP Rust SDK), `axum` + `tokio`, `serde`, `schemars`, `ulid`, `clap`, `tracing`; `keyring` only for the opt-in keychain (§6).

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
  embed_state TEXT, judge_state TEXT
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
       target_type TEXT, target_id TEXT, before_json TEXT, after_json TEXT)
review_items(id TEXT PK, type TEXT, payload_json TEXT, confidence REAL,
             status TEXT, created_at TEXT, resolved_at TEXT, resolved_by TEXT, resolution TEXT)
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

Migrations are forward-only, numbered, and run at daemon start inside a transaction after an automatic pre-migration backup.

---

## 4. Storage and retrieval

### 4.1 Storage

- SQLite in WAL mode; `synchronous=NORMAL`, `busy_timeout=5000`, `foreign_keys=ON`, no memory map (`mmap_size=0`: each connection's map counts towards RSS separately, and reads through the OS file cache measured the same speed), allocation statistics off (they serialise every allocation behind one mutex).
- One writer connection (writer thread, 64 MiB page cache); a pool of at most 8 read connections (16 MiB page cache each) for concurrent queries. Reads beyond 8 wait for a free connection, which bounds memory however many requests arrive at once.
- The vector index (`usearch` HNSW, cosine, int8 quantization) is two indexes per model, one for memories and one for chunks, saved as `vectors/<model>.memories.usearch` and `vectors/<model>.chunks.usearch` (each written atomically; an unchanged part is not rewritten). A memory search walks only the memory graph, so it never pays for the (much larger) chunk graph. Both are fully rebuildable from the `embeddings` table. On start-up (and daily, M2) the index count is compared with the table; on mismatch or load failure the index is rebuilt on a background thread while search uses the lexical lists only.

### 4.2 Embeddings

- Default: `multilingual-e5-small-q8`, the int8 ONNX build of `intfloat/multilingual-e5-small` (384-d) from `Xenova/multilingual-e5-small` at a pinned revision: `onnx/model_quantized.onnx` (118 MB) plus `tokenizer.json` (17 MB) and three small config files, run by ONNX Runtime (linked statically) with mean pooling and `query: ` / `passage: ` prefixes. Good Thai and English quality, no API key. One loaded instance costs about 600 MB (the tokenizer of the 250k-token vocabulary ~250 MB, the ONNX session ~265 MB), so the daemon keeps one, shared by the background worker and queries: passages are embedded in batches of at most 1,024 padded tokens, counted with the model's tokenizer (32 texts at most) and the model is handed to a waiting query after each batch, so a query waits for one batch at most (~115 ms for two 512-token texts on an M4 Pro). The cap also bounds ONNX Runtime's working memory, which keeps the size of its largest call (32 texts of 512 tokens: +1.4 GB).
- Providers: `local` (ONNX), `ollama`, `openai` (any OpenAI-compatible endpoint), `voyage`.
- Each vector records its model. Changing the model triggers a background re-embed into a new index; search keeps using the old index until the new one is complete, then switches atomically.
- Model files are downloaded on first use to `models/<model>/` with size and SHA-256 verification (`HF_ENDPOINT` selects a mirror); `alice model fetch` pre-downloads for offline machines.
- Embedding completion is compare-and-set: the worker stores a vector only if the row's `content_hash` still equals the one it read with the text, so an edit that lands while the old text is being embedded is never marked done with the old vector.
- Local embeddings need an ONNX Runtime prebuilt library: Apple Silicon macOS, glibc Linux (x86_64, aarch64) and Windows x64. Intel macOS and musl Linux have none; they need an HTTP embedding provider (M2). (M1 status: the Windows build compiles but does not link, because usearch 2.26.4 references its NumKong SIMD kernels through DLL-import names under MSVC.)

### 4.3 Lexical search

- FTS5 `unicode61` for space-delimited languages. English stopwords are dropped from word queries whenever the query has other words (a query of stopwords only keeps them), so a lexical match is a content-word match; regular English inflections of query words are added (`replies` → `reply`, `deploy` → `deploys`, `deployed`), since `unicode61` does not stem. Every term is quoted, so FTS5 syntax in a query is searched as literal words.
- FTS5 `trigram` for Thai, CJK and substring matches (Thai has no word spaces, so word tokenizers miss it).
- `code_tokens` column: at index time, identifiers and paths are expanded (`HostLinkReconnect` → `host link reconnect`, `writer_lease` → `writer lease`, `internal/teacher/jev/client.go` → path segments and stem). Each name in a token (split at `/ . :`) also yields the joined runs of its consecutive words (split at `_`, `-` and camel case; at most 12 words, runs of up to 6), so every written form of an identifier carries its joined token: `self.lease_token_frame`, `store/lease_token_frame.rs`, `crate::daemon::lease_token_frame()`, `LEASE_TOKEN_FRAME_LIMIT`, `lease-token-frame` and `LeaseTokenFrame` all hold `leasetokenframe`. Kebab-case counts as an identifier. Exact identifier/path queries also get a dedicated exact-match candidate list, searched by the whole joined identifier and by the joined form of each multi-word name in it.

### 4.4 Ranking pipeline

1. **Candidates:** vector top 100; `unicode61` BM25 top 100; trigram top 100 (always for non-space scripts or queries ≤ 2 words, otherwise when the other lists are short); exact identifier/path matches; a `LIKE` scan for Thai terms shorter than a trigram. With filters (project, kind, topic, dates) each list takes 400. Only the first 2,000 characters of a query are searched and embedded. Raw-tier lists bound their cost per term (ranking every match of a word found in 40% of 1M chunks took ~300 ms). In the word and identifier lists, every term that has a match gets its **own share** of the budget, max(k, 5,000 / number of such terms) rows (k is 100, or 400 with filters), and reads its newest matches up to that share; a chunk scores the sum of the BM25 parts of every term that read it, and the best k are kept. Rows read per list are at most max(5,000, terms × k): 5,000 for a few terms, up to 6,400 for the word list's 64 terms (32 words and 32 variants), or 25,600 with filters. The raw word list does not search identifiers holding `_` as phrases of their words (BM25's IDF for a phrase of common words reads every match of each word with positions, 36–50 ms per identifier at 1M chunks); they are found as one token in the identifier list, in every written form, and their words stay in the word list. Likewise words unicode61 splits at combining marks (half of common Thai words: `ตัดสินใจ` becomes `ต`, `ดส`, `นใจ`, a phrase of common fragments whose IDF read 25–31 ms at 1M chunks) are left to the trigram list, which searches them whole. In the trigram list, a non-ASCII term (Thai, CJK, ...; the trigram list is its main lexical list) reads its newest max(k, 5,000 / number of non-ASCII terms) matches, and an ASCII term its newest k substring matches (as a whole word it has its own share in the word list, as an identifier in the identifier list); a chunk scores Σ 1 / (each matching term's count, capped at what it read), and ties are taken term by term (BM25's IDF needs every match of each trigram phrase, 60–400 ms at 1M chunks). **Guarantee:** no term can crowd another out of the raw tier, because every term is read up to its own share; a term with at most its share of matches is ranked over all of them; matches beyond a term's share (and an ASCII term's substring-only matches beyond its newest k) are not read, newest first, so recall for very common terms is not exhaustive. A share costs ~5–16 ms at 1M chunks for a single word; with identifiers and split words out of the word list, phrase terms are left only for rare punctuation inside a word.
2. **Fusion:** Reciprocal Rank Fusion, k = 60.
3. **Boosts** (multiplicative, all configurable):
   - tier: knowledge 1.0, raw 0.6
   - project: current 1.3, `global` 1.1, other 1.0
   - recency: `0.7 + 0.3 · exp(−age / half_life(kind))`, half-lives: `status` 7 d, `handoff` 14 d, `diary` 30 d, `note` 90 d, `fact`/`howto`/`bugfix`/`summary` 180 d, `decision`/`preference`/`warning` no decay
   - importance: `0.9 + 0.05 · importance`
   - usage: up to +10 % by `log(1 + access_count)`
   - status: `superseded` 0.3, `archived` 0.5 (still retrievable)
4. **Precise mode (optional):** the Judge scores the top 20 for relevance in one batched call and results are re-ordered by P(relevant).
5. **Raw fallback:** a result is *relevant* if it has a lexical match (BM25, trigram or exact) or a cosine similarity ≥ `search.relevance_floor` (default 0.82 for `multilingual-e5-small-q8`, checked in M1 with the real model on the gold set: as the int8 index reports similarity, every gold query–memory pair scores ≥ 0.841 and every unrelated pair ≤ 0.771; `crates/alice-embed/tests/real_search_quality.rs`). If `include_raw` is false and fewer than `search.raw_fallback_min_results` (default 3) knowledge results are relevant, relevant raw results are appended and labelled `raw`.
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

M1 measurements are in [docs/benchmarks/m1.md](../../benchmarks/m1.md): every latency budget is met at 1M chunks (`include_raw` recall p95 166 ms; 160 ms under a filter, 143 ms for a rare identifier, 41 ms for a rare Thai word, 89 ms for the raw fallback). The memory budget (RSS) is not: without the embedding model the daemon at 1M chunks holds 666–784 MB RSS and peaks at 860 MB; with the local model it is projected at ≈1.4 GB RSS steady and ≈1.6–1.7 GB RSS at peak (physical footprint ≈1.4–1.5 GB steady, ≈1.55 GB peak); one model instance alone is ~600 MB. At 100k chunks with the model the peak RSS is 910–1,003 MB (footprint ~0.85 GB).

---

## 5. Write path, Judge and maintenance

### 5.1 Write path (`remember`, hook deliveries, imports)

1. **Normalize:** resolve the project via aliases, normalize topics, default `kind` to `note` (refined later by the Judge if enabled).
2. **Redact secrets** (§5.3) before anything touches disk.
3. **Exact dedup:** if `content_hash` exists in the project, return the existing id with `status: duplicate`. This makes retries idempotent. An optional client `idempotency_key` covers non-identical retries.
4. **Insert** the memory (status `pending`) and its FTS rows in one transaction; **acknowledge** with the id. The memory is immediately findable by lexical search.
5. **Background pipeline:** embed → add to vector index → Judge: compare with the 5 nearest memories in the project (§5.2) → link mentioned entities (alias match; new entities proposed by the Judge when enabled) → set status `active`.

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

- `heuristic` (default, offline): duplicate if cosine ≥ 0.97 in the same project; an "updates" candidate if the same project, kind and near-identical title are newer; kind from keyword rules; predicate match by normalized string similarity (Jaro-Winkler ≥ 0.9 after stemming). Heuristic results above the auto threshold apply only for duplicates; everything else goes to review.
- `jev` (optional): TypeSafe Jev (`https://api.typesafe.ai/v1/systemone`), model pinned to `jev-1.13.0`, key from the OS keychain or `TYPESAFE_API_KEY`.
  - Numbers are verbalized before sending (Jev reads words better than numbers): "yesterday", "two weeks ago", "importance high".
  - Choice options are shuffled per call; for `compare` decisions that may supersede, the question is asked in forward and reversed order in the same request and the distributions averaged.
  - Responses are cached by SHA-256 of the request; retries only on 429, 5xx and timeouts; rate ≤ 10 requests/s.
  - Daily budget in USD with a hard stop; every call recorded in `cost_ledger`.
  - Never sends projects listed in `privacy.cloud_denied_projects`; redaction always runs first.

SP2 adds `ollama`, `claude-cli`, `codex-cli` and `openai-compatible` providers.

**Judge tasks in SP1**

| Task | Question | Action |
|---|---|---|
| Relation to existing | For each of the 5 nearest: duplicate / updates / contradicts / unrelated | duplicate → the older memory survives and gains the new one's provenance; the new one becomes `trashed` with a `duplicate_of` link, and `get_memory` on its id returns the survivor; updates → new `supersedes` old (old becomes `superseded`); contradicts → `contradicts` link + review item |
| Kind | Which kind fits this memory? | Set `kind` when the writer did not |
| Project routing | Which existing project does this belong to? (only when cwd/roots are unknown) | Set `project_id` |
| Predicate normalization | Which existing predicate does this one mean? | Map to canonical predicate or create a new one |
| Precise recall | How well does each result answer the query? | Re-rank |

**Thresholds:** auto-apply when P ≥ `judge.auto_apply_threshold` (default 0.85); create a review item when P ≥ `judge.review_threshold` (default 0.5); otherwise do nothing. If the Judge fails, times out or exceeds its budget, the heuristic runs instead and the memory is marked `judge_state = retry` for a later pass. A Judge failure never fails a write.

### 5.3 Secret redaction

- Built-in detectors: AWS keys, GCP service-account keys, GitHub tokens (`ghp_`, `gho_`, `github_pat_`), GitLab tokens, Slack (`xox[abprs]-`), Stripe (`sk_live_`, `rk_live_`), OpenAI (`sk-`), Anthropic (`sk-ant-`), JWTs, PEM private keys, credentials in URLs (`scheme://user:pass@`), `password|secret|token|api_key` assignments, and high-entropy strings ≥ 32 characters in assignment context.
- Matches are replaced with `[REDACTED:<type>]`. Mode `block` rejects the write instead, with a message naming the detector.
- User-defined patterns and allowlists in config.
- Redaction applies to memories, raw chunks, Judge requests, sync exports and logs.

### 5.4 Review inbox

Review items have a type (`supersede`, `contradiction`, `merge`, `predicate`, `project`, `sync_conflict`), a payload, a confidence and a status. They are resolved from the UI (SP3), `alice review`, or the admin MCP toolset. Resolutions are recorded as events.

### 5.5 History and undo

Every mutation writes an `events` row with actor (`agent:<name>`, `user`, `judge:<provider>`, `sync`, `import`), action, target and before/after JSON. `alice undo <event-id>` reverses an event when the target has not changed since; otherwise it creates a review item. Deletes move memories to `trashed`; trash is purged after 30 days.

### 5.6 Maintenance scheduler

Runs in a low-priority worker when the daemon has been idle for 60 s, and yields as soon as requests arrive.

| Job | Default frequency |
|---|---|
| Backup via SQLite online backup API; keep 7 daily + 4 weekly in `backups/` | Daily |
| `PRAGMA quick_check`; vector index count check and rebuild if needed | Daily |
| FTS `optimize` | Weekly |
| Raw retention (`forever` default; or by age, e.g. `90d`; or by size, e.g. `10GB`) | Daily |
| Similar-memory clusters → merge suggestions in review | Weekly |
| Trash purge (> 30 days) | Daily |
| Judge retries for `judge_state = retry` | Hourly |
| Spool drain | Every 5 s and at start-up |

The database file is never stored in git. Restore with `alice restore [backup]`, which stops writes, swaps files and rebuilds indexes.

### 5.7 Error handling

| Failure | Behaviour |
|---|---|
| Daemon unreachable | Writes go to the spool; reads return a short error telling the agent memory is starting and to retry in a few seconds. |
| Embedding provider fails | Memory stays lexically searchable; `embed_state = retry` with exponential backoff; health shows the backlog. |
| Judge fails / over budget | Heuristic fallback; `judge_state = retry`. |
| Vector index missing or inconsistent | Rebuild on a background thread; lexical search only meanwhile (`health.index_ready = false`). |
| Disk full, DB corruption, failed migration | Read-only mode. Writes go to the spool. Agents receive: `Memory is read-only: <reason>. Run "alice doctor".` |
| Client timeouts | `recall` 5 s, `context` 3 s; writes acknowledge immediately. No 120 s waits. |

All MCP and CLI errors state what happened and what to do next, in one or two sentences.

---

## 6. Settings

- **One schema** defined as Rust structs (`serde` + `schemars`). The JSON Schema is served at `GET /api/v1/settings/schema`; the UI form (SP3), `alice config` and the reference docs are generated from it.
- **Layers** (later wins): built-in defaults → `~/.alice/config.toml` → `<repo>/.alice.toml` (project scope) → environment variables (`ALICE_<SECTION>_<KEY>`).
- **Live reload:** changes through the API or edits to the files (watched) apply without restarting the daemon. Invalid values are rejected with the schema error; the previous value stays in effect.
- **Secrets** (Jev key, embedding API keys, git credentials if not using the system helper) are stored by default in `~/.alice/secrets.toml` with mode 0600. The OS keychain via `keyring` (macOS Keychain, Windows Credential Manager, Secret Service on Linux) is opt-in (`secrets.backend = "keychain"`): on macOS every rebuilt or updated binary is a new identity to the Keychain and blocks on a GUI permission prompt, which a background daemon cannot answer, and on Linux the Secret Service needs a D-Bus session that headless machines often lack. The API never returns secret values, only `{set: true, hint: "••••ab12"}`.
- **API:** `GET /api/v1/settings`, `PATCH /api/v1/settings`, `PUT /api/v1/secrets/{name}`, `DELETE /api/v1/secrets/{name}`, `POST /api/v1/settings/test/{provider}` (connection test).
- **CLI:** `alice config get|set|list|edit`, `alice config set-secret <name>` (reads from stdin, never from argv).

Groups and main keys:

```toml
[storage]       data_dir, raw_retention
[embedding]     provider, model, endpoint, batch_size
[search]        default_max_tokens, relevance_floor, raw_fallback_min_results, [search.weights] …, [search.half_life] …
[capture]       per-agent enable flags (used by SP2)
[judge]         provider, auto_apply_threshold, review_threshold, daily_budget_usd, timeout_ms
[judge.jev]     model, endpoint, rate_per_sec
[distill]       provider, model (used by SP2)
[privacy]       redact, redact_mode, custom_patterns, allowlist, cloud_denied_projects
[sync]          remote, branch, auto, include_raw, excluded_projects
[backup]        daily, weekly, dir
[compat]        mempalace
[server]        port (0 = random), open_browser
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

**Admin toolset** (disabled by default; `mcp.admin_tools = true`): `review_list`, `review_resolve`, `merge_memories`, `stats`.

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
alice recall <query> [--project p] [--kind k] [--raw] [--precise] [--json]
alice remember [--kind k] [--title t] [--topic t]... [--project p] (<text> | -)
alice context [--project p]
alice get <id>...           alice facts <entity> [--as-of date] [--timeline]
alice review [list|resolve <id> <choice>]
alice undo <event-id>
alice sync [init <remote>|push|pull|status]
alice import mempalace [--palace path] [--dry-run] [--since date]
alice config [get|set|list|edit|set-secret]
alice doctor                alice bench
alice ui                    alice model fetch
alice daemon [start|stop|status|logs]
alice service [install|uninstall]
alice mcp                   alice hook <event>   (SP2)
alice token rotate
```

Human-readable tables by default, `--json` for scripts. The CLI supports every write operation, so agents without MCP access can use Alice through shell commands.

### 8.4 HTTP API

`/api/v1/` JSON endpoints for memories, search, context, facts, projects, topics, graph (nodes/edges with filters and paging), reviews, events/undo, settings/secrets, sync, health and stats. OpenAPI generated from the handlers. `GET /api/v1/events/stream` (SSE) pushes changes, review items, sync status and health to the UI.

### 8.5 Contracts for SP2 and SP3

- **SP2 hooks:** `alice hook <event>` reads the host's hook JSON on stdin, sends it to the daemon (or the spool) and prints host-specific output (for SessionStart, the `context` briefing as additional context). Must finish in < 100 ms. Transcript ingestion uses `POST /api/v1/sources` with chunk batches; distillation results use `remember` with provenance.
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
- **Concurrency regression (the MemPalace failure):** 20 clients writing and searching concurrently while an import of 100k records runs. Pass: `remember` p95 < 50 ms, `recall` p95 < 200 ms, zero lost or duplicated writes.
- **Crash safety:** `kill -9` during writes, during import and during sync; on restart no acknowledged write is lost (spool + WAL); a deleted or corrupted vector index rebuilds automatically.
- **Sync:** two simulated devices and a bare remote: concurrent edits, deletes on one side, edit-vs-delete, push races, KG merges; verify deterministic files and review items.
- **Search quality:** gold query sets scored with nDCG@10 and Recall@10. A synthetic multilingual fixture (Thai, English, mixed, code identifiers) lives in the repo and runs in CI. A gold set built from the owner's palace runs locally only and is never committed. The Jev Judge can act as relevance grader when enabled.
- **Interfaces:** MCP conformance with a reference client, CLI golden tests (snapshot), HTTP auth tests (missing token, wrong Host, foreign Origin).
- **Security:** redaction corpus covering every built-in detector; log scanning for secrets in tests.
- **Benchmarks:** `alice bench` and Criterion suites for write acknowledgement, search at 100k and 1M synthetic chunks, import throughput and RSS. Results recorded per release.

CI: GitHub Actions on macOS, Linux and Windows (fmt, clippy `-D warnings`, tests, synthetic search eval).

---

## 11. Distribution

- `cargo-dist` builds release binaries with checksums for the targets ONNX Runtime ships prebuilt libraries for: macOS arm64, Linux glibc x64 and arm64, Windows x64. There are no musl or Intel macOS builds with local embeddings (no prebuilt ONNX Runtime); those targets wait for the HTTP embedding providers (M2) and then ship without the local model.
- Install paths: Homebrew tap (`brew install icueth/tap/alice`), shell/PowerShell installer scripts, npm wrapper (`npm i -g alice-memory`), `cargo install alice-memory`.
- Binary name `alice`; package name `alice-memory` on crates.io and npm (`alice` is taken on both).
- `alice self-update`; update checks are opt-in.
- License MIT. Credits MemPalace in the README and in any file that adapts its code.

---

## 12. Milestones and acceptance criteria (SP1)

| Milestone | Delivers | Accepted when |
|---|---|---|
| **M1 — Store, search, interfaces** | Workspace, schema + migrations, daemon (auto-start, lock, token, spool), write path steps 1–4 + embedding/indexing, hybrid search with Thai/code support, `context`, 7 MCP tools, CLI, HTTP API skeleton | Concurrency regression and crash-safety tests pass; performance budgets met at 100k synthetic; Claude Code and Codex can use Alice over MCP |
| **M2 — Judge, maintenance, settings** | Judge layer (heuristic + Jev), relation-to-existing, review inbox, events/undo, redaction, scheduler jobs, backups/restore, settings schema/API/live reload/keychain, health | Jev fixtures and live smoke test pass; budget stop verified; settings changes apply live; restore round-trip verified |
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
| Raw-tier ranking (M1 benchmarks and review) | Every term with a match reads its newest max(k, 5,000 / terms) matches and every row read is summed; `_` identifiers searched as one token in the identifier list, which holds the joined form of every name; trigrams: non-ASCII terms read the same share, ASCII terms their newest k; ties taken term by term | Ranking every match of a common word cost 300–400 ms per list at 1M chunks (`include_raw` p95 947 ms). A window shared by all terms let a common term hide a rarer one (review finding I1); the controller ruled that every term gets its own share and that matches beyond it are taken newest-first. Keeping only each term's own top k before summing lost chunks holding every query word (review N1); dropping `_` identifiers from the word list needed every written form indexed with the joined token (review N2, controller ruling) |
