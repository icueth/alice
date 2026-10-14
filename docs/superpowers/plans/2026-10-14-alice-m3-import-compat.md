# Alice M3 — MemPalace Importer and Compatibility Layer — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship Alice milestone M3: `alice import mempalace` reads a MemPalace palace (`sqlite_exact` or `chroma` backend plus `knowledge_graph.sqlite3` and tunnels) from a read-only snapshot and imports it completely, resumably and incrementally into Alice's two tiers, projects, topics and knowledge graph; an opt-in `mempalace_*` MCP compatibility layer keeps existing prompts working; the import report says exactly what happened.

**Architecture:** Pure mapping logic lives in a new `alice-core::import` module (palace readers, wing → project map, tier/kind classification, survey, KG and tunnel mapping, report); one new store module `store::import` plus one `WriteOp::ImportBatch` apply a bounded batch (memories, source chunks, facts, links, review items) in a single short writer transaction; `alice-daemon::importer` is the worker thread that snapshots, surveys, redacts, batches and checkpoints; `alice-daemon::compat` translates `mempalace_*` calls onto the `Service`; the MCP server lists and routes those tools only while `compat.mempalace` is true. Nothing in M2's behaviour changes.

**Tech Stack:** Rust 1.99 (edition 2024), rusqlite 0.40 (bundled, `backup` feature), serde_json, chrono, strsim (existing `judge::text::predicate_similarity`), rmcp 3.5.1, axum 0.8, clap 4.6. No new crates except enabling rusqlite's `backup` feature (M2 Task 14 enables the same feature; a merge conflict on that one `Cargo.toml` line resolves to the union).

**Spec:** `docs/superpowers/specs/2026-10-13-alice-core-design.md` — §9 (importer), §8.2 (compat layer), §12 row M3 (acceptance), and what they rely on: §3.2–3.3 (data model, `legacy_id`), §5.1 (write path), §5.3 (redaction), §5.4 (review inbox), §5.5 (events), §8.1/§8.3/§8.4 (tools, CLI, HTTP), §10 (concurrency and crash tests, search-quality gold sets). Where the spec and this plan disagree the spec wins unless the "Plan decisions" section at the end says otherwise.

**Base:** branch `feat/m3-import`, created from `feat/m2-judge-settings` at 403c14f (M2 Tasks 1–9 done, Task 10 in re-review, Tasks 11–18 not started). Names in this plan are those of that commit. **M2 keeps moving**: at every task boundary run `git merge --no-edit feat/m2-judge-settings` if it moved, resolve conflicts, re-run the tests. **First step of every task:** read every file the task modifies as it is now; when a signature named here has changed, keep the behaviour this plan asks for and adapt the call. M3 never changes M2 behaviour; if M3 needs an M2 change, write it in the ledger and tell the controller.

**Real data seen while planning (a copy of the owner's palace, structure only, no content):** `sqlite_exact` backend; collections `mempalace_drawers` (119,179 documents) and `mempalace_closets` (4,416, dropped); metadata JSON per document with keys `wing`, `room`, `hall`, `source_file`, `added_by`, `ingest_mode` (`convos` | `registry` | absent), `extract_mode`, `chunk_index`, `filed_at`, `authored_at`, `agent`/`date`/`topic`/`type` (diary entries), `parent_drawer_id`; 302 wings (`sessions` 76,169 transcript drawers from 130 `.jsonl` files under `~/.claude/projects/<encoded cwd>/`; 218 agent-style `wing_*` wings with 1,851 drawers); 5,006 diary drawers (room `diary`, no `source_file`, ids `diary_wing_<agent>_<time>_<hash>`); 34,600 file-mined drawers with `added_by` set to an agent name (code chunks, so `added_by` alone does NOT mean knowledge); document text ≤ 10,981 chars (median 800); knowledge graph: 266 entities (all type `unknown`), 159 triples (2 ended). Embeddings are MiniLM 384-d and are ignored.

## Global Constraints

- Toolchain: Rust stable 1.99, edition 2024, `resolver = "3"`; crates `alice-core`, `alice-embed`, `alice-daemon`, binary package `alice-memory` (`[[bin]] name = "alice"`), `license = "MIT"`.
- IDs are ULIDs, timestamps RFC 3339 UTC with milliseconds and `Z` (`alice_core::ids::now_rfc3339`), HLC from the writer's `Hlc`.
- Performance budgets (spec §4.6) never relax: `remember` ack p95 < 50 ms and `recall` fast p95 < 200 ms **while an import runs** (Task 12 proves it); daemon RSS < 800 MB at 1M chunks.
- Only the daemon opens `alice.db`; all writes go through the single writer thread as `WriteOp`s. Importing parses and maps on a worker thread and hands the writer **bounded batches**: ≤ 64 KB of text and ≤ 200 records (memories + chunks + facts + links) per `ImportBatch`, so a `remember` queued behind an import waits for one batch at most (spec §2.2 rule 3).
- Limits from M1 stand (content ≤ 65,536 bytes, title ≤ 200 chars, ≤ 20 topics, chunk ≤ 32,768 bytes, importance 1–5). An imported drawer over a limit is split or truncated by the importer and counted in the report; it is never silently dropped and never an error.
- Redaction (spec §5.3) runs on every imported text field (title, body, chunk text, topics) **before** it reaches the writer; text already redacted is left as it is; in `block` mode a drawer holding a secret is skipped and reported (detector names only), the import goes on. No secret appears in `alice.db`, the report, the checkpoint or a log.
- A Judge failure never fails a write; imports do not call the Judge. Imported memories are stored with `judge_state = 'skipped'` so the M2 pipeline never sends the owner's history to a cloud provider. Embedding runs in the background after the import (lexical search works at once).
- Error text for agents and CLI users: one or two sentences saying what happened and what to do next.
- Server binds 127.0.0.1 only; every new route sits behind the existing bearer-token, `Host` and `Origin` guard.
- MCP core tool names stay `recall`, `remember`, `context`, `get_memory`, `update_memory`, `query_facts`, `assert_fact`; compat tools are named exactly as MemPalace names them (`mempalace_*`) and exist only while `compat.mempalace` is true.
- **The owner's real palace is read-only.** It lives at `/Users/icue/Code/AI/memory_vault` (backend `sqlite_exact`; config `~/.mempalace/config.json`; KG `~/.mempalace/knowledge_graph.sqlite3`). Never write, lock, migrate or delete anything there or under `~/.mempalace`. Any run against real data uses a copy made with the SQLite backup API into the session scratch directory and a throwaway `ALICE_HOME`. **Tests use synthetic fixtures built in code, never the real palace** (code, not committed binaries).
- **No cloud with real data.** Real palace content is never sent to Jev or any external service. If an acceptance step seems to need it, stop and ask the owner (via the controller) first.
- Never read or print `/Users/icue/Code/AI/project-alice/.env` or `TYPESAFE_API_KEY` (exported in this shell). Run every cargo test with `env -u TYPESAFE_API_KEY`. Secret-shaped test strings are built with `concat!` split inside the provider prefix (e.g. `concat!("gh", "p_…")`, `concat!("sk-", "ant-…")`); GitHub push protection rejects them otherwise.
- Heavy commands: `source "$HOME/.cargo/env"` first; `nice -n 15 cargo build|test|clippy -j 4`; one heavy run at a time; never run the 1M benches or real-model evaluations (the controller's lane owns those).
- Scratch prelude for every manual or shell step (`S` is this session's scratchpad; a later session substitutes its own): `export S=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice-m3/9facbaac-88e0-459a-9167-5ca357c0ce3e/scratchpad ALICE_HOME=$S/m3-home ALICE_EMBEDDER=fake; mkdir -p "$ALICE_HOME"`. The palace copy made with the backup API on 2026-10-14 20:50 is `$S/pal/` (`sqlite_exact.sqlite3`, `knowledge_graph.sqlite3`); Task 14 refreshes it. Never use the real `~/.alice` or the real palace path in a command.
- Never edit `~/.claude*`, `~/.codex` or any agent config; no release, no publish, no tag. Push only `feat/m3-import`; never push to or merge into `main` or `feat/m2-judge-settings`.
- Process tests leave no stray daemons: after every test run `pgrep -fl "alice daemon run"` prints nothing the run started.
- The workspace builds and its tests pass after **every** task, ignored tests included (`cargo test --workspace` compiles `#[ignore]` tests): a task that changes an API updates every caller.
- Before every commit: the task's tests, `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`. Stage files by name (never `git add -A`). Commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Every implementer confirms each named test exists with `git show --stat` + `grep -c "fn <test_name>"` before reporting (earlier lanes had silent edit failures: rustfmt moved the anchor and the edit did nothing).
- A shell hook routes commands through `rtk`; when output looks filtered or truncated, prefix `rtk proxy`. Docs are English; `README.th.md` changes in the same commit as `README.md`.

## Review Focus

The spec says what the importer must do; it is silent on the inputs below, and silence is not permission to break.

1. **Secrets inside imported history.** Transcripts and code chunks hold tokens, `.env` lines and passwords. Expected: every imported text field is redacted before the writer sees it; `block` mode skips that drawer and counts it (detector only); the report, the checkpoint and the logs never contain the matched text. Owned by Task 8 (test with `concat!`-built secrets in a transcript chunk, a diary and a code chunk).
2. **The palace is live and the palace is precious.** MemPalace may be writing (WAL file, hub lease), the path may be a symlink, missing, not a palace, a `chroma` directory with no `chroma.sqlite3`, or damaged. Expected: the source is opened read-only, a snapshot is taken with the backup API into a private temp dir, the source's files are byte-identical afterwards, and a bad path gives one clear sentence — never a panic, never a partial import. Owned by Tasks 2, 3 and 8.
3. **Interrupted and repeated imports.** `kill -9` mid-batch, running the import twice, running it while the palace gained drawers, `--since` with a bad date. Expected: every committed batch stands, a re-run finishes the rest, a run with nothing new imports zero records and creates zero duplicates, new drawers are picked up, a bad date is refused before any work. Owned by Tasks 7, 8 and 12.
4. **Odd drawers.** Empty or whitespace-only text, text over 64 KB / 32 KB, NUL bytes, decomposed Thai sara am, missing `wing`/`room`/`chunk_index`, duplicate or negative `chunk_index`, the same text filed under `pg-ai`, `pg_ai` and `wing_pg_ai`, timestamps with and without zone, a wing that slugifies to nothing. Expected: each is imported sensibly or skipped with a counted reason; none is an error. Owned by Tasks 4 and 5.
5. **Old prompts hitting the compat layer.** Unknown wing, a `drawer_id` of a raw chunk or of a collapsed duplicate, missing required arguments, numbers sent as strings, calls while `compat.mempalace` is off, tools Alice does not provide, `wing_pg_ai` as an alias of `pg-ai`. Expected: a helpful MemPalace-shaped answer (never a stack trace), the Alice equivalent named for unsupported tools, aliases resolved. Owned by Tasks 10 and 11.

---

## File Structure

```
Cargo.toml                                   rusqlite + "backup" (M2 Task 14 adds the same)
crates/alice-core/Cargo.toml                 feature `fixtures` (test-only palace builders, off by default)
crates/alice-core/src/
  lib.rs                                     + pub mod import
  import/mod.rs                              ImportOptions, ImportRequest, locate_palace, tz offset (Tasks 2, 8)
  import/palace.rs                           Drawer, DrawerMeta, PalaceReader, snapshot, sqlite_exact reader (Task 2)
  import/chroma.rs                           chroma reader (Task 3)
  import/wings.rs                            WingMap: wing/path → project, agent wings, aliases, inference (Task 4)
  import/plan.rs                             classify, kind_for_room, Survey, Planner, Planned (Task 5)
  import/kg.rs                               KG + tunnels reader and mapping (Task 6)
  import/report.rs                           ImportReport, text rendering (Task 8)
  import/fixture.rs                          (feature `fixtures`) palace/KG/tunnel builders (Tasks 2, 3, 6)
  store/sql/000N_import.sql                  legacy_ids table (N = next free number after merging M2; 0004 expected) (Task 1)
  store/import.rs                            legacy_ids access, ImportBatch, apply_batch (Tasks 1, 7)
  store/projects.rs                          adopt an `import:` placeholder project (Task 1)
crates/alice-daemon/src/
  importer.rs                                ImportManager: job thread, survey, redact, batch, checkpoint, status (Task 8)
  compat.rs                                  mempalace_* → Service translation (Task 10)
  compat_schema.rs                           the tools' names, descriptions and JSON input schemas (Task 11)
  writer.rs / service.rs / http.rs / mcp.rs / client.rs   WriteOp::ImportBatch, Service import+compat methods, routes, MCP gating, shim forwarding
crates/alice/src/cmd.rs, main.rs             `alice import mempalace` (Task 9)
crates/alice-daemon/tests/
  import_flow.rs                             end-to-end import of a fixture palace (Tasks 8, 9)
  compat_flow.rs                             compat calls and MCP listing (Tasks 10, 11)
  import_concurrency.rs                      20 clients during an import (Task 12)
  import_quality.rs, data/gold_corpus.json, data/mempalace_baseline.json   gold-set comparison (Task 13)
scripts/m3_mempalace_baseline.py             local-only: builds a throwaway MemPalace palace from gold_corpus.json and records MemPalace's search results (Task 13)
docs/benchmarks/m3.md                        import throughput, RSS, quality (Tasks 12–14)
docs/superpowers/handoff/…-m3-handoff.md     (Task 14)
```

## Task list

| # | Task | Depends on |
|---|---|---|
| 1 | Foundations: `legacy_ids` table and access, `import:` placeholder projects adopted by the real project | — |
| 2 | Palace snapshot (backup API) and the `sqlite_exact` reader, with a fixture builder | — |
| 3 | The `chroma` reader | 2 |
| 4 | Wings → projects: normalisation, agent wings, transcript paths, content inference, overrides | 2 |
| 5 | Tier, kind, topics, author, time and size mapping; the survey (ords, collapse); the planner | 2, 4 |
| 6 | Knowledge graph, predicates and tunnels | 2 |
| 7 | `WriteOp::ImportBatch`: one bounded transaction for memories, chunks, facts, links and reviews | 1, 5, 6 |
| 8 | The import job: snapshot → survey → redact → batch → checkpoint → report; HTTP routes | 3, 7 |
| 9 | `alice import mempalace` CLI and the automatic opt-in of the compat layer | 8 |
| 10 | Compat core: every supported `mempalace_*` call on the Service, unsupported ones answered | 1, 8 |
| 11 | Compat on MCP: schemas, live gating by `compat.mempalace`, stdio shim forwarding | 10, M2 Task 11 |
| 12 | Concurrency and crash safety during an import | 8 |
| 13 | Gold-set quality: synthetic corpus, recorded MemPalace baseline, nDCG@10 / Recall@10 | 8, 10 |
| 14 | Acceptance on a copy of the owner's palace, docs, spec as built, handoff | all |

---

### Task 1: Foundations — `legacy_ids` and placeholder projects

**Why:** `memories.legacy_id` is a single column, but spec §9 collapses duplicates ("every original id is stored in `legacy_id` for compat lookups") and the compat layer must resolve a collapsed drawer's old id to the survivor; raw chunks have `chunks.legacy_id` but no index (a 1M-row scan per lookup). One small table solves both and doubles as the "already imported" index for incremental runs. Imported projects exist before any checkout does; when an agent later works in the real checkout, the real project must **adopt** the placeholder instead of becoming `pg-ai-1a2b3c` (that would re-create the wing sprawl the product exists to remove).

**Files:**
- Create: `crates/alice-core/src/store/sql/000N_import.sql` (N = next free number; `ls crates/alice-core/src/store/sql` and `MIGRATIONS` after merging M2; the M2 plan reserves 0002 and 0003, so 0004 is expected — if M2 added more, use the next one)
- Create: `crates/alice-core/src/store/import.rs`
- Modify: `crates/alice-core/src/store/migrations.rs` (register the file, bump `CURRENT_VERSION`), `crates/alice-core/src/store/mod.rs` (`pub mod import;`), `crates/alice-core/src/store/projects.rs` (`upsert_project`)
- Test: unit tests inside `store/import.rs` and `store/projects.rs`

**Interfaces:**
- Produces:
  - `store::import::LegacyTarget { pub target_type: String, pub target_id: String }` (`target_type` ∈ `memory` | `chunk` | `relation`)
  - `fn legacy_put(conn: &Connection, legacy_id: &str, target_type: &str, target_id: &str) -> Result<()>` (INSERT OR IGNORE)
  - `fn legacy_get(conn: &Connection, legacy_id: &str) -> Result<Option<LegacyTarget>>`
  - `fn legacy_existing(conn: &Connection, ids: &[String]) -> Result<HashSet<String>>` (chunks of ≤ 500 ids per query)
  - `const PLACEHOLDER_KEY_PREFIX: &str = "import:"`; `store::projects::upsert_project` adopts a placeholder.

- [ ] **Step 1: Write the failing tests** (in `store/import.rs` `mod tests`; helper `fresh()` opens an in-memory migrated DB the way the neighbouring `store/*` tests do — read `store/reviews.rs` tests for the helper name and copy its use)

```rust
#[test]
fn legacy_ids_round_trip_and_ignore_repeats() {
    let conn = fresh();
    legacy_put(&conn, "drawer_a", "memory", "M1").unwrap();
    legacy_put(&conn, "drawer_a", "memory", "OTHER").unwrap(); // first writer wins
    let hit = legacy_get(&conn, "drawer_a").unwrap().unwrap();
    assert_eq!((hit.target_type.as_str(), hit.target_id.as_str()), ("memory", "M1"));
    assert!(legacy_get(&conn, "nope").unwrap().is_none());
}

#[test]
fn existing_finds_only_stored_ids_even_for_thousands() {
    let conn = fresh();
    for i in 0..1200 { legacy_put(&conn, &format!("d{i}"), "chunk", &format!("C{i}")).unwrap(); }
    let ask: Vec<String> = (1100..1300).map(|i| format!("d{i}")).collect();
    let got = legacy_existing(&conn, &ask).unwrap();
    assert_eq!(got.len(), 100);
    assert!(got.contains("d1199") && !got.contains("d1200"));
}
```

and in `store/projects.rs` `mod tests`:

```rust
#[test]
fn a_real_project_adopts_the_import_placeholder_of_the_same_slug() {
    let conn = fresh();
    let placeholder = ProjectRef { key: "import:pg-ai".into(), slug: "pg-ai".into(), name: "pg-ai".into(), root: None, remote: None };
    let imported = upsert_project(&conn, &placeholder).unwrap();
    let real = ProjectRef { key: "github.com/icueth/pg-ai".into(), slug: "pg-ai".into(), name: "PG AI".into(), root: None, remote: Some("github.com/icueth/pg-ai".into()) };
    let adopted = upsert_project(&conn, &real).unwrap();
    assert_eq!(adopted.id, imported.id, "same project, no pg-ai-<hash> twin");
    assert_eq!(adopted.slug, "pg-ai");
    assert_eq!(find_by_key(&conn, "github.com/icueth/pg-ai").unwrap().unwrap().id, imported.id);
    assert!(find_by_key(&conn, "import:pg-ai").unwrap().is_none());
}

#[test]
fn a_placeholder_is_not_adopted_by_another_placeholder_or_a_different_slug() {
    let conn = fresh();
    let a = upsert_project(&conn, &ProjectRef { key: "import:alpha".into(), slug: "alpha".into(), name: "alpha".into(), root: None, remote: None }).unwrap();
    let other = upsert_project(&conn, &ProjectRef { key: "path:/x/beta".into(), slug: "beta".into(), name: "beta".into(), root: None, remote: None }).unwrap();
    assert_ne!(a.id, other.id);
    // a second import: key identical → the same project
    let again = upsert_project(&conn, &ProjectRef { key: "import:alpha".into(), slug: "alpha".into(), name: "alpha".into(), root: None, remote: None }).unwrap();
    assert_eq!(again.id, a.id);
}
```

- [ ] **Step 2: Run to verify they fail**

`source "$HOME/.cargo/env"; env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-core store::import store::projects -j 4`
Expected: FAIL (unresolved `legacy_put`, no adoption).

- [ ] **Step 3: Implement.** Migration file:

```sql
-- Alice schema v{N}: where each imported legacy id (a MemPalace drawer id, a KG triple id) went.
-- Collapsed duplicates point at the survivor, so compat lookups and incremental imports work.
CREATE TABLE legacy_ids (
  legacy_id   TEXT PRIMARY KEY,
  target_type TEXT NOT NULL,        -- memory | chunk | relation
  target_id   TEXT NOT NULL
) WITHOUT ROWID;
```

`store/import.rs`:

```rust
use crate::error::Result;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use std::collections::HashSet;

pub struct LegacyTarget { pub target_type: String, pub target_id: String }

pub fn legacy_put(conn: &Connection, legacy_id: &str, target_type: &str, target_id: &str) -> Result<()> {
    conn.execute("INSERT OR IGNORE INTO legacy_ids (legacy_id, target_type, target_id) VALUES (?1, ?2, ?3)",
        params![legacy_id, target_type, target_id])?;
    Ok(())
}

pub fn legacy_get(conn: &Connection, legacy_id: &str) -> Result<Option<LegacyTarget>> {
    Ok(conn.query_row("SELECT target_type, target_id FROM legacy_ids WHERE legacy_id = ?1", [legacy_id],
        |r| Ok(LegacyTarget { target_type: r.get(0)?, target_id: r.get(1)? })).optional()?)
}

pub fn legacy_existing(conn: &Connection, ids: &[String]) -> Result<HashSet<String>> {
    let mut found = HashSet::new();
    for part in ids.chunks(500) {
        let marks = vec!["?"; part.len()].join(",");
        let mut stmt = conn.prepare(&format!("SELECT legacy_id FROM legacy_ids WHERE legacy_id IN ({marks})"))?;
        for row in stmt.query_map(params_from_iter(part.iter()), |r| r.get::<_, String>(0))? { found.insert(row?); }
    }
    Ok(found)
}
```

`upsert_project`: after the `find_by_key` early return and before the slug-conflict logic, when `!p.key.starts_with(PLACEHOLDER_KEY_PREFIX)` look for `SELECT id, slug, name FROM projects WHERE slug = ?1 AND key LIKE 'import:%'` with `slugify(&p.slug)`; if found, `UPDATE projects SET key = ?1, name = ?2, git_remote = ?3, root_path = ?4 WHERE id = ?5` (root as in the existing branch) and return it. Placeholders never adopt placeholders (the early `find_by_key` already returns the same project for the same key).

- [ ] **Step 4: Run the tests** — same command. Expected: PASS. Also `nice -n 15 cargo test -p alice-core -j 4` (the migration count assertion in `migrations.rs` tests, if any, must be updated in this step; the daemon tests that open fresh databases must still pass: `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --lib -j 4`).
- [ ] **Step 5: Confirm the named tests landed** (`git show --stat HEAD` after commit, `grep -c "fn legacy_ids_round_trip_and_ignore_repeats\|fn a_real_project_adopts" …` = 2 in the right files), then fmt, clippy, commit:

```bash
git add crates/alice-core/src/store/sql crates/alice-core/src/store/import.rs crates/alice-core/src/store/mod.rs crates/alice-core/src/store/migrations.rs crates/alice-core/src/store/projects.rs
git commit -m "feat: legacy_ids table and adoption of import placeholder projects"
```

---

### Task 2: Palace snapshot and the `sqlite_exact` reader

**Files:**
- Create: `crates/alice-core/src/import/mod.rs`, `crates/alice-core/src/import/palace.rs`, `crates/alice-core/src/import/fixture.rs`
- Modify: `crates/alice-core/src/lib.rs` (`pub mod import;`), `crates/alice-core/Cargo.toml` (`[features] fixtures = []`), root `Cargo.toml` (rusqlite features `["bundled", "backup"]`), `crates/alice-daemon/Cargo.toml` and `crates/alice/Cargo.toml` (`[dev-dependencies] alice-core = { path = "../alice-core", features = ["fixtures"] }`)
- Test: `crates/alice-core/src/import/palace.rs` (`mod tests`)

**Interfaces:**
- Produces (in `alice_core::import::palace`):

```rust
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DrawerMeta {
    pub wing: String, pub room: String, pub hall: Option<String>,
    pub source_file: Option<String>, pub added_by: Option<String>,
    pub ingest_mode: Option<String>, pub filed_at: Option<String>, pub authored_at: Option<String>,
    pub chunk_index: Option<i64>, pub agent: Option<String>, pub date: Option<String>,
    pub topic: Option<String>, pub entry_type: Option<String>, pub parent_drawer_id: Option<String>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Drawer { pub id: String, pub text: String, pub meta: DrawerMeta }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend { SqliteExact, Chroma }
#[derive(Debug, Clone)]
pub struct PalaceInfo { pub backend: Backend, pub path: std::path::PathBuf, pub drawers: u64 }
pub struct Snapshot { pub dir: std::path::PathBuf, pub backend: Backend, pub source: std::path::PathBuf }
pub trait PalaceReader: Send {
    fn info(&self) -> &PalaceInfo;
    /// Drawers with id greater than `after` (byte order), at most `limit`, optionally only those filed at or after `since`.
    fn page(&self, after: Option<&str>, limit: usize, since: Option<&str>) -> Result<Vec<Drawer>>;
    /// Drawers by id (missing ids are skipped).
    fn get(&self, ids: &[String]) -> Result<Vec<Drawer>>;
    /// (wing, drawer count) over the whole palace.
    fn wing_counts(&self) -> Result<Vec<(String, u64)>>;
}
pub fn detect_backend(palace: &Path) -> Result<Backend>;
pub fn snapshot(palace: &Path, dest_dir: &Path) -> Result<Snapshot>;
pub fn open_snapshot(s: &Snapshot) -> Result<Box<dyn PalaceReader>>;
```
  `import::fixture` (feature `fixtures`): `pub struct DrawerSpec { id, text, meta: DrawerMeta }` and `pub fn build_sqlite_exact(dir: &Path, drawers: &[DrawerSpec]) -> PathBuf` that creates `dir/sqlite_exact.sqlite3` with exactly the MemPalace tables (`meta`, `collections`, `documents`, `docs_fts` optional) and the collection `mempalace_drawers`, one closets row, `journal_mode=DELETE`.

- [ ] **Step 1: Write the failing tests.** The fixture builder is the first deliverable (tests need it):

```rust
// fixture.rs (cfg feature = "fixtures")
pub fn build_sqlite_exact(dir: &Path, drawers: &[DrawerSpec]) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("sqlite_exact.sqlite3");
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE collections (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL UNIQUE, dimension INTEGER, created_at TEXT NOT NULL);
         CREATE TABLE documents (collection_id INTEGER NOT NULL, id TEXT NOT NULL, document TEXT NOT NULL, metadata_json TEXT NOT NULL,
            embedding BLOB NOT NULL, dim INTEGER NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY (collection_id, id));
         INSERT INTO collections (name, dimension, created_at) VALUES ('mempalace_drawers', 384, '2026-01-01T00:00:00+00:00'), ('mempalace_closets', 384, '2026-01-01T00:00:00+00:00');
         INSERT INTO documents VALUES (2, 'closet_1', 'closet text', '{\"wing\":\"x\"}', x'00', 1, '2026-01-01', '2026-01-01');",
    ).unwrap();
    for d in drawers {
        c.execute("INSERT INTO documents VALUES (1, ?1, ?2, ?3, x'00', 1, '2026-01-01', '2026-01-01')",
            rusqlite::params![d.id, d.text, meta_json(&d.meta)]).unwrap();
    }
    path
}
```
(`meta_json` builds the JSON object with only the `Some` keys, so tests can model missing metadata.) Then in `palace.rs` tests:

```rust
#[test]
fn snapshot_leaves_the_source_byte_identical_and_reads_every_drawer() {
    let src = tempfile::tempdir().unwrap();
    build_sqlite_exact(src.path(), &sample(250));
    let before = hash_dir(src.path());
    let dst = tempfile::tempdir().unwrap();
    let snap = snapshot(src.path(), dst.path()).unwrap();
    assert_eq!(hash_dir(src.path()), before, "the palace must not change");
    assert_eq!(snap.backend, Backend::SqliteExact);
    let r = open_snapshot(&snap).unwrap();
    assert_eq!(r.info().drawers, 250); // closets are not counted
    let mut seen = Vec::new(); let mut after: Option<String> = None;
    loop { let p = r.page(after.as_deref(), 100, None).unwrap(); if p.is_empty() { break; }
        after = Some(p.last().unwrap().id.clone()); seen.extend(p.into_iter().map(|d| d.id)); }
    assert_eq!(seen.len(), 250);
    let mut sorted = seen.clone(); sorted.sort(); assert_eq!(seen, sorted, "pages are in id order");
}

#[test]
fn since_keeps_only_drawers_filed_at_or_after_the_date() { /* 3 drawers filed 2026-01-01 / 2026-06-01 / no filed_at → since 2026-03-01 returns only the second; a drawer with no filed_at is kept (cannot be shown older) */ }

#[test]
fn metadata_is_parsed_with_missing_keys_and_wrong_types() { /* chunk_index stored as "7" (string) and 7.0 both → Some(7); wing missing → "" ; hall null → None */ }

#[test]
fn bad_paths_say_what_to_do() {
    let d = tempfile::tempdir().unwrap();
    let e = snapshot(&d.path().join("missing"), d.path()).unwrap_err().to_string();
    assert!(e.contains("not a MemPalace palace") || e.contains("does not exist"), "{e}");
    std::fs::write(d.path().join("sqlite_exact.sqlite3"), b"not sqlite").unwrap();
    let dst = tempfile::tempdir().unwrap();
    let e = snapshot(d.path(), dst.path()).unwrap_err().to_string();
    assert!(e.contains("sqlite_exact.sqlite3") && e.contains("palace"), "{e}");
}

#[test]
fn a_symlinked_palace_directory_is_followed_and_still_not_modified() { /* unix only: symlink dir → fixture; snapshot ok; hash unchanged */ }
```

- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-core import:: -j 4`. Expected: FAIL (module missing).
- [ ] **Step 3: Implement.**
  - `detect_backend`: `sqlite_exact.sqlite3` present → `SqliteExact`; else `chroma.sqlite3` → `Chroma`; else `Err(AliceError::invalid("<path> is not a MemPalace palace (no sqlite_exact.sqlite3 or chroma.sqlite3); pass the folder that holds it with --palace"))`. A missing directory: "`<path>` does not exist; pass --palace <folder> or run `mempalace status` to see where your palace is".
  - `snapshot`: open the source file with `Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX)` (never `create`), create the destination in `dest_dir` (mode 0700 dir), run `rusqlite::backup::Backup::new(&src, &mut dst)?.run_to_completion(2000, Duration::from_millis(5), None)` (pages per step 2000 so a writer is never starved). Map open/`not a database` errors to "`<file>` cannot be read as a SQLite database ({e}); is the palace damaged? Copy of nothing was made". Snapshot also copies `knowledge_graph.sqlite3` when it sits in the palace dir (same method), recording nothing else. Nothing but those files is read; no `-wal`/`-shm` file is created next to the source by Alice (the backup API reads through SQLite's own WAL handling; the test asserts the directory listing is unchanged for the non-WAL fixture, and Task 14's real-copy step asserts it for the WAL case).
  - The reader: collection id by name `mempalace_drawers` (configurable through `config.json` `collection_name`, read by `locate_palace` in Task 8; default constant `DEFAULT_COLLECTION`); `page` = `SELECT id, document, metadata_json FROM documents WHERE collection_id = ?1 AND id > ?2 [AND (json_extract(metadata_json,'$.filed_at') IS NULL OR json_extract(metadata_json,'$.filed_at') >= ?3)] ORDER BY id LIMIT ?4`; `get` uses a chunked `IN (...)`; `wing_counts` = `SELECT json_extract(metadata_json,'$.wing'), count(*) … GROUP BY 1` (NULL wing → `""`). Meta parsing is tolerant: `chunk_index` accepts int, float or numeric string; any non-string scalar for text keys is stringified; JSON that fails to parse yields default meta plus the id (the drawer is then classified/skipped by Task 5, never an error). NUL bytes in `document` are kept here (Task 5 strips them).
- [ ] **Step 4: Run the tests** — Expected: PASS. Add `nice -n 15 cargo build -p alice-core --features fixtures -j 4`.
- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/alice-core/Cargo.toml crates/alice-core/src/lib.rs crates/alice-core/src/import crates/alice-daemon/Cargo.toml crates/alice/Cargo.toml
git commit -m "feat: palace snapshot and sqlite_exact reader"
```

---

### Task 3: The `chroma` reader

**Files:**
- Create: `crates/alice-core/src/import/chroma.rs`
- Modify: `crates/alice-core/src/import/palace.rs` (`snapshot`/`open_snapshot` dispatch on `Backend::Chroma`), `crates/alice-core/src/import/fixture.rs` (`build_chroma`)
- Test: `crates/alice-core/src/import/chroma.rs` (`mod tests`)

**Interfaces:**
- Consumes: `PalaceReader`, `Drawer`, `DrawerMeta`, `snapshot` (Task 2).
- Produces: `pub struct ChromaReader` implementing `PalaceReader`; `fixture::build_chroma(dir, &[DrawerSpec]) -> PathBuf` (creates `dir/chroma.sqlite3`).

Facts verified against chromadb 1.5.7 while planning: only `chroma.sqlite3` is needed (documents and metadata live in SQLite; the HNSW folders are vectors and are ignored). Tables: `collections(id TEXT PK, name, dimension, database_id, …)`; `segments(id, type, scope, collection)` where the metadata segment has `scope = 'METADATA'`; `embeddings(id INTEGER PK, segment_id, embedding_id TEXT, seq_id BLOB, created_at, UNIQUE(segment_id, embedding_id))`; `embedding_metadata(id INTEGER REFERENCES embeddings(id), key TEXT, string_value TEXT, int_value INTEGER, float_value REAL, bool_value INTEGER, PRIMARY KEY(id, key))`; the document text is the metadata key `chroma:document` (string_value). A drawer's id is `embeddings.embedding_id`; its metadata is the other keys of its `embedding_metadata` rows.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn chroma_drawers_come_out_like_sqlite_exact_ones() {
    let src = tempfile::tempdir().unwrap();
    let specs = sample_specs(120);
    build_chroma(src.path(), &specs);
    let dst = tempfile::tempdir().unwrap();
    let snap = snapshot(src.path(), dst.path()).unwrap();
    assert_eq!(snap.backend, Backend::Chroma);
    let r = open_snapshot(&snap).unwrap();
    assert_eq!(r.info().drawers, 120);
    let mut all = Vec::new(); let mut after = None;
    loop { let p = r.page(after.as_deref(), 50, None).unwrap(); if p.is_empty() { break; } after = Some(p.last().unwrap().id.clone()); all.extend(p); }
    assert_eq!(all.len(), 120);
    let a = all.iter().find(|d| d.id == specs[7].id).unwrap();
    assert_eq!(a.text, specs[7].text);
    assert_eq!(a.meta, specs[7].meta); // int chunk_index, string wing, missing keys stay None
}

#[test]
fn a_chroma_folder_without_the_drawers_collection_names_the_collections_it_has() {
    /* build_chroma with collection "other" → open_snapshot → Err containing "mempalace_drawers" and "other" */
}

#[test]
fn since_and_wing_counts_work_on_chroma_too() { /* same assertions as the sqlite_exact tests, via the shared trait */ }
```

- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-core import::chroma -j 4`. Expected: FAIL.
- [ ] **Step 3: Implement.** `build_chroma` creates the five tables above with the exact DDL printed in "Facts verified" (plus a `databases(id)` row so the FK is satisfied), one collection `mempalace_drawers`, a `VECTOR` and a `METADATA` segment, and one `embeddings` row + `embedding_metadata` rows per spec (`chroma:document`, then `wing`/`room`/… as `string_value`, `chunk_index` as `int_value`). `ChromaReader::page`: `SELECT e.id, e.embedding_id FROM embeddings e JOIN segments s ON s.id = e.segment_id WHERE s.scope='METADATA' AND s.collection = ?1 AND e.embedding_id > ?2 ORDER BY e.embedding_id LIMIT ?3`, then one query loading all `embedding_metadata` rows of those internal ids and folding them into `Drawer`/`DrawerMeta` with the same tolerant typing as Task 2 (shared helper `meta_from_pairs(impl Iterator<Item=(String, MetaValue)>)` lives in `palace.rs`; use it from both readers so they cannot drift). `since` filters in Rust after the fold for Chroma when the `filed_at` key is a string (ISO strings compare lexicographically); keep paging until `limit` drawers pass the filter or the cursor ends. `info().drawers` = count of embeddings of the metadata segment of the drawers collection. If the collection is missing: `Err(invalid("chroma palace has no collection \"mempalace_drawers\" (it has: {names}); set collection_name in the palace's config or pass --collection"))`. (The CLI flag `--collection` is added in Task 9; the reader takes the collection name as a parameter with default `DEFAULT_COLLECTION`.)
- [ ] **Step 4: Run the tests** — PASS; run the whole `alice-core` suite.
- [ ] **Step 5: Commit**

```bash
git add crates/alice-core/src/import/chroma.rs crates/alice-core/src/import/palace.rs crates/alice-core/src/import/fixture.rs crates/alice-core/src/import/mod.rs
git commit -m "feat: chroma palace reader"
```

### Task 4: Wings → projects

**Why:** 302 wings become a handful of projects. `pg-ai`, `pg_ai` and `wing_pg_ai` are one project; `wing_claude_opus_4.7`, `wing_impl_t5`, `wing_pane4` are agents' scratch wings, not projects; `sessions` holds every project's transcripts, whose project is encoded in the transcript's folder (`~/.claude/projects/-Users-icue-Code-payments-quickpay-pg-ai/…jsonl`). The rules below are data-driven (they use the palace's own wing list) so they work on any palace, not just the owner's.

**Files:**
- Create: `crates/alice-core/src/import/wings.rs`
- Modify: `crates/alice-core/src/import/mod.rs` (`pub mod wings;`)
- Test: `crates/alice-core/src/import/wings.rs` (`mod tests`)

**Interfaces:**
- Consumes: `normalize::slugify`, `import::palace::DrawerMeta` (Task 2).
- Produces:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WingTarget {
    /// A project wing (or an alias of one).
    Project { slug: String },
    /// An agent's own wing (`wing_claude_opus_4_7`): the project is inferred per drawer; `agent` is the wing suffix as a slug.
    Agent { agent: String },
    /// The `sessions` wing: transcripts, project from the transcript path.
    Sessions,
    /// The wing slugifies to nothing.
    Unnamed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Via { Wing, Alias, Override, TranscriptPath, Inferred, Global }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectDecision { pub slug: String, pub via: Via, pub agent: Option<String> }
pub struct WingMap { /* private */ }
impl WingMap {
    pub fn build(wings: &[(String, u64)], overrides: &std::collections::BTreeMap<String, String>) -> WingMap;
    pub fn target(&self, wing: &str) -> WingTarget;
    /// Project slugs (not agents) with the wing spellings that map to each: slug → wings.
    pub fn projects(&self) -> std::collections::BTreeMap<String, Vec<String>>;
    /// alias (as `slugify` gives it) → project slug, for every spelling that differs from the slug.
    pub fn aliases(&self) -> Vec<(String, String)>;
    pub fn decide(&self, meta: &DrawerMeta, text: &str) -> ProjectDecision;
    pub fn project_for_transcript(&self, source_file: &str) -> Option<String>;
}
```

**Rules (all pinned by tests below):**
1. `canon(w) = slugify(w)`. Empty → `Unnamed` (drawers go to `global`, via `Global`).
2. `sessions` (canon) → `Sessions`.
3. A wing whose canon starts with `wing-` is *prefixed*. Its stem is the canon without that prefix. If the stem equals the canon of any non-prefixed wing (other than `sessions`) the wing is an **alias** of that project (`Project`, via `Alias`); otherwise it is `Agent { agent: stem }`.
4. A non-prefixed wing is `Project { slug: canon }` (via `Wing`).
5. `overrides` (`--map-wing wing=project`) win: the key is matched against the wing exactly or by canon; the value is slugified; via `Override`. An override whose project is not otherwise known creates that project.
6. `decide(meta, text)`: `Project` → its slug. `Agent` → `infer(text)` (below) else `global` (via `Inferred` / `Global`), `agent = Some(stem)` either way. `Sessions` or any drawer with a transcript `source_file` → `project_for_transcript(source_file)` else the wing rule else content inference else `global`.
7. `infer(text)`: look at the first 4,000 chars lower-cased; for each known project slug of ≥ 4 characters count whole-token occurrences of its three spellings (`pg-ai`, `pg_ai`, `pgai`); the project with the strictly highest count wins; a tie or no hit → none. "Whole token" = the neighbours are not ASCII alphanumeric.
8. `project_for_transcript(path)`: take the folder name (parent of the file, e.g. `-Users-icue-Code-AI-devspace`), split on `-`, lower-case; the known project whose slug tokens are a **suffix** of those tokens wins, longest slug first. If none matches, and the tokens contain `code`, the tokens after the first `code` (dropping a leading `ai` when something follows) joined with `-` become a new project slug (listed by `projects()` as coming from transcripts). Otherwise `None`.

- [ ] **Step 1: Write the failing tests**

```rust
fn counts(ws: &[&str]) -> Vec<(String, u64)> { ws.iter().map(|w| (w.to_string(), 1)).collect() }

#[test]
fn spellings_of_one_project_collapse_and_agent_wings_stay_out() {
    let m = WingMap::build(&counts(&["pg-ai", "pg_ai", "wing_pg_ai", "wing_claude_opus_4.7", "wing_impl_t5", "sessions", "devspace", "wing_devspace"]), &Default::default());
    assert_eq!(m.target("pg_ai"), WingTarget::Project { slug: "pg-ai".into() });
    assert_eq!(m.target("wing_pg_ai"), WingTarget::Project { slug: "pg-ai".into() });
    assert_eq!(m.target("wing_devspace"), WingTarget::Project { slug: "devspace".into() });
    assert_eq!(m.target("wing_claude_opus_4.7"), WingTarget::Agent { agent: "claude-opus-4-7".into() });
    assert_eq!(m.target("wing_impl_t5"), WingTarget::Agent { agent: "impl-t5".into() });
    assert_eq!(m.target("sessions"), WingTarget::Sessions);
    let slugs: Vec<_> = m.projects().keys().cloned().collect();
    assert_eq!(slugs, vec!["devspace", "pg-ai"]);
    assert!(m.aliases().contains(&("wing-pg-ai".into(), "pg-ai".into())));
}

#[test]
fn an_override_beats_the_rules_and_can_name_a_new_project() {
    let mut o = std::collections::BTreeMap::new();
    o.insert("wing_project_alice".to_string(), "alice".to_string());
    let m = WingMap::build(&counts(&["alice", "wing_project_alice"]), &o);
    assert_eq!(m.target("wing_project_alice"), WingTarget::Project { slug: "alice".into() });
    let d = m.decide(&DrawerMeta { wing: "wing_project_alice".into(), ..Default::default() }, "x");
    assert_eq!((d.slug.as_str(), d.via), ("alice", Via::Override));
}

#[test]
fn an_agent_wing_takes_the_project_its_text_mentions_most() {
    let m = WingMap::build(&counts(&["pg-ai", "devspace", "wing_claude_code"]), &Default::default());
    let meta = DrawerMeta { wing: "wing_claude_code".into(), ..Default::default() };
    let d = m.decide(&meta, "Fixed the pg_ai v4 frame bug; pgai tests pass. (devspace unaffected)");
    assert_eq!((d.slug.as_str(), d.via, d.agent.as_deref()), ("pg-ai", Via::Inferred, Some("claude-code")));
    let tie = m.decide(&meta, "pg-ai and devspace once each");
    assert_eq!((tie.slug.as_str(), tie.via), ("global", Via::Global));
    let none = m.decide(&meta, "nothing relevant, mentions spg-aix only");
    assert_eq!(none.slug, "global"); // "spg-aix" is not a whole token
}

#[test]
fn transcript_folders_decode_to_projects() {
    let m = WingMap::build(&counts(&["pg-ai", "games-platform", "alice", "sessions"]), &Default::default());
    let p = |f: &str| m.project_for_transcript(f);
    assert_eq!(p("/Users/x/.claude/projects/-Users-icue-Code-payments-quickpay-pg-ai/a.jsonl").as_deref(), Some("pg-ai"));
    assert_eq!(p("/Users/x/.claude/projects/-Users-icue-Code-games-platform/a.jsonl").as_deref(), Some("games-platform"));
    assert_eq!(p("/Users/x/.claude/projects/-Users-icue-Code-AI-project-alice/a.jsonl").as_deref(), Some("alice"));
    // unknown project: taken from the folder name after `code`, `ai` dropped
    assert_eq!(p("/Users/x/.claude/projects/-Users-icue-Code-AI-gsb-app-verify/a.jsonl").as_deref(), Some("gsb-app-verify"));
    assert_eq!(p("/tmp/loose/a.jsonl"), None);
}

#[test]
fn a_sessions_drawer_without_a_decodable_path_falls_back_to_its_text_then_global() {
    let m = WingMap::build(&counts(&["pg-ai", "sessions"]), &Default::default());
    let meta = DrawerMeta { wing: "sessions".into(), source_file: Some("/tmp/loose/a.jsonl".into()), ..Default::default() };
    assert_eq!(m.decide(&meta, "talking about pg-ai here").slug, "pg-ai");
    assert_eq!(m.decide(&meta, "talking about nothing").slug, "global");
}

#[test]
fn odd_wing_names_do_not_panic() {
    let m = WingMap::build(&counts(&["", "   ", "---", "wing_", "ไทย", "Wing-Mixed_Case"]), &Default::default());
    assert_eq!(m.target(""), WingTarget::Unnamed);
    assert_eq!(m.target("---"), WingTarget::Unnamed);
    assert!(matches!(m.target("ไทย"), WingTarget::Project { .. }));
    let d = m.decide(&DrawerMeta { wing: "   ".into(), ..Default::default() }, "x");
    assert_eq!(d.slug, "global");
}
```

- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-core import::wings -j 4`. Expected: FAIL.
- [ ] **Step 3: Implement** the rules above. Keep the wing → target table in a `HashMap<String, WingTarget>` keyed by the **raw** wing string (so `target()` is an exact lookup; unknown wings seen later by `decide` are classified on the fly with the same rules against the canon set built at `build`). `wing_` / `wing-` prefix test works on `canon` (`starts_with("wing-")`); a wing named exactly `wing_` has an empty stem → `Unnamed`. `project_for_transcript` returns slugs as stored in `projects()`; a slug first seen through a transcript is added to an interior `RefCell`-free `Vec` built at `build` time by scanning `wings` only — to learn transcript-derived projects the planner calls `WingMap::note_transcript_dir(&mut self, source_file)` during the survey (Task 5), which inserts the derived slug into the project table with wing spelling `transcript:<folder>`.
- [ ] **Step 4: Run the tests** — PASS.
- [ ] **Step 5: Commit** — `git add crates/alice-core/src/import/wings.rs crates/alice-core/src/import/mod.rs && git commit -m "feat: map MemPalace wings and transcript folders to projects"`

---

### Task 5: Classification, mapping, survey and planner

**Why:** this is the mapping table of spec §9, made exact against the real palace shape. It is pure (no database, no writer): drawers in, planned memories and chunks out.

**Files:**
- Create: `crates/alice-core/src/import/plan.rs`
- Modify: `crates/alice-core/src/import/mod.rs` (`pub mod plan;`, `pub fn local_offset() -> chrono::FixedOffset`)
- Test: `crates/alice-core/src/import/plan.rs` (`mod tests`)

**Interfaces:**
- Consumes: `PalaceReader`, `Drawer`, `DrawerMeta` (Task 2); `WingMap`, `ProjectDecision`, `Via` (Task 4); `store::memories::NewMemoryInput`, `store::sources::{SourceInput, ChunkInput}`, `model::{Kind, SourceType, Author, limits}`, `normalize::{nfc, normalize_topic, sha256_hex}`.
- Produces:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tier { Knowledge, RawSession, RawFile, Skip(&'static str) }
pub fn classify(id: &str, meta: &DrawerMeta) -> Tier;
pub fn kind_for_room(room: &str) -> Kind;
pub fn parse_time(s: &str, assume: chrono::FixedOffset) -> Option<String>; // RFC 3339 ms Z

#[derive(Debug, Default)]
pub struct Survey {
    pub total: u64,
    pub wing_counts: Vec<(String, u64)>,
    pub knowledge_order: Vec<String>,              // knowledge drawer ids, earliest filed first
    pub ords: std::collections::HashMap<String, i64>,
    pub collapsed: std::collections::HashMap<String, String>, // raw drawer id → keeper drawer id
    pub source_spans: std::collections::HashMap<String, (Option<String>, Option<String>)>, // uri → (first, last authored_at)
    pub skipped: std::collections::BTreeMap<&'static str, u64>,
    pub tiers: TierCounts, // knowledge, raw_session, raw_file
}
pub fn survey(reader: &dyn PalaceReader, since: Option<&str>, wings: &mut WingMap, assume: chrono::FixedOffset) -> Result<Survey>;

#[derive(Debug, Clone)]
pub struct PlannedMemory { pub legacy_id: String, pub project: ProjectDecision, pub input: NewMemoryInput }
#[derive(Debug, Clone)]
pub struct PlannedChunk { pub legacy_id: String, pub project: ProjectDecision, pub source: SourceInput, pub source_type: SourceType, pub chunk: ChunkInput }
#[derive(Debug, Clone)]
pub enum Planned {
    Memories(Vec<PlannedMemory>),   // several when an oversize drawer was split
    Chunks(Vec<PlannedChunk>),      // several when an oversize chunk was split
    Skipped { reason: &'static str },
    Collapsed { keeper: String },
}
pub struct Planner<'a> { pub wings: &'a WingMap, pub survey: &'a Survey, pub assume: chrono::FixedOffset }
impl Planner<'_> { pub fn plan(&self, d: &Drawer) -> Planned; }
```

**Rules (pinned by tests):**
1. `classify` in this order: `ingest_mode == registry`, `room == _registry` or id starts `_reg_` → `Skip("MemPalace registry entry")`; `ingest_mode == convos`, or `source_file` ends `.jsonl`, or wing canon `sessions` → `RawSession`; room `diary`, `entry_type == diary_entry` or id starts `diary_` → `Knowledge`; no `source_file` → `Knowledge`; room ∈ {`decisions`, `knowledge`, `status`, `progress`, `runbook`, `methods`, `bug-classes`, `issues`} **and** `added_by` set and ≠ `mempalace` → `Knowledge` (deliberate widening of spec §9, Plan decision 2); everything else `RawFile`. Text that is empty after trimming (NUL bytes removed) → `Skip("empty text")` (checked by `plan`, since `classify` sees no text).
2. `kind_for_room` (room slugified): `decisions` → Decision; `bug-classes`, `issues`, `bugs` → Bugfix; `methods`, `runbook` → Howto; `status`, `progress` → Status; `diary` → Diary; `knowledge` → Fact; anything else → Note.
3. Knowledge memory: `content` = text with NUL removed, NFC; `topics` = [room slug] (diary: [`topic` slug] when present, else none); `importance` 3 (diary 2); `author.agent` = `meta.agent` (diary) else `added_by` unless it is `mempalace`, else the `Agent` wing suffix; `created_at` = `parse_time(filed_at)` else `parse_time(date)` else none; `legacy_id` = drawer id; `title` = `None` (derived from the first line) except diary: `format!("Diary {date}: {topic}")` when both exist (≤ 200 chars). Text over 65,536 bytes is split at paragraph (`\n\n`), else line, else char boundary into parts of ≤ 60,000 bytes; part *k* (k ≥ 2) has `legacy_id` `"<id>#<k>"` and title suffix ` (part k/n)`.
4. Raw chunk: `source` for `RawSession` = type Session, `uri` `file://<source_file>` (absolute paths) else the string as is; no `source_file` → `mempalace-wing:<wing>`; `agent` = `claude-code` if the path contains `/.claude/`, `codex` if it contains `/.codex/`, else none; `session_id` = file stem; `started_at`/`ended_at` from `survey.source_spans`; `RawFile` = type File, same uri rule. `chunk.ord` = `survey.ords[id]`; `chunk.authored_at` = `parse_time(authored_at)` (RawFile: `parse_time(filed_at)`); `chunk.legacy_id` = drawer id; project from `wings.decide`. Text over 32,768 bytes is split on char boundaries into ≤ 32,000-byte pieces; piece *k* ≥ 2 has `ord + k*10_000_000` and `legacy_id` `"<id>#<k>"`.
5. A raw drawer in `survey.collapsed` plans `Collapsed { keeper }` (same source, identical text; earliest `(chunk_index, filed_at, id)` kept).
6. `parse_time`: accepts RFC 3339 with `Z` or an offset, naive `YYYY-MM-DDTHH:MM:SS[.ffffff]` (interpreted in `assume`, the machine's UTC offset at import time — MemPalace writes naive local times), and bare `YYYY-MM-DD` (midnight in `assume`); anything else → `None`. Output `…T…​.mmmZ`.
7. `survey` pages the whole reader (5,000 per page, `since` applied), classifies each drawer, hashes raw text (first 16 bytes of SHA-256), collapses `(uri, hash)` repeats, assigns ords per source exactly as: sort rows by `(chunk_index.unwrap_or(i64::MAX), filed_at, id)`; a row whose `chunk_index` is ≥ 0 and not yet claimed gets it; all other rows get `max_claimed + 1, +2, …` in order. It records each source's first/last authored time, the knowledge order `(filed_at, id)`, and calls `wings.note_transcript_dir` for each transcript path. Memory: ≈ 100 bytes per raw drawer (≈ 12 MB for the owner's palace, ≈ 100 MB at 1M drawers).

- [ ] **Step 1: Write the failing tests** (a `d(id, text, meta)` helper builds `Drawer`s; `fixture::build_sqlite_exact` + `snapshot` + `open_snapshot` feeds `survey`)

```rust
#[test]
fn classification_follows_the_real_palace_shapes() {
    let m = |f: &dyn Fn(&mut DrawerMeta)| { let mut x = DrawerMeta::default(); f(&mut x); x };
    assert_eq!(classify("drawer_sessions_technical_ab", &m(&|x| { x.wing="sessions".into(); x.ingest_mode=Some("convos".into()); x.source_file=Some("/h/.claude/projects/-a/s.jsonl".into()); x.added_by=Some("mempalace".into()); })), Tier::RawSession);
    assert_eq!(classify("diary_wing_claude-code_20260416_x", &m(&|x| { x.room="diary".into(); x.entry_type=Some("diary_entry".into()); })), Tier::Knowledge);
    assert_eq!(classify("drawer_test_general_x", &m(&|x| { x.added_by=Some("claude".into()); })), Tier::Knowledge, "agent-written, no source file");
    assert_eq!(classify("drawer_devspace_src_x", &m(&|x| { x.added_by=Some("claude".into()); x.source_file=Some("/p/src/a.ts".into()); x.room="src".into(); })), Tier::RawFile, "added_by alone is not knowledge");
    assert_eq!(classify("drawer_pg-ai_decisions_x", &m(&|x| { x.added_by=Some("claude".into()); x.source_file=Some("/p/notes.md".into()); x.room="decisions".into(); })), Tier::Knowledge);
    assert_eq!(classify("_reg_8", &m(&|x| { x.room="_registry".into(); x.ingest_mode=Some("registry".into()); })), Tier::Skip("MemPalace registry entry"));
}

#[test]
fn rooms_become_kinds_and_topics() {
    assert_eq!(kind_for_room("decisions"), Kind::Decision);
    assert_eq!(kind_for_room("Bug_Classes"), Kind::Bugfix);
    assert_eq!(kind_for_room("runbook"), Kind::Howto);
    assert_eq!(kind_for_room("progress"), Kind::Status);
    assert_eq!(kind_for_room("architecture"), Kind::Note);
}

#[test]
fn a_diary_drawer_becomes_a_diary_memory_with_its_agent_topic_and_date() {
    /* meta: room diary, agent claude-code, topic "system-maintenance", date 2026-04-16, filed_at 2026-04-16T11:34:59.427116, wing wing_claude_code
       → Planned::Memories([one]); kind Diary, title "Diary 2026-04-16: system-maintenance", topics ["system-maintenance"], importance 2,
         author.agent Some("claude-code"), created_at Some("2026-04-16T..Z") shifted by the assumed offset (use FixedOffset::east(7*3600) → 04:34:59.427Z), legacy_id the drawer id */
}

#[test]
fn naive_and_zoned_times_parse_and_junk_is_none() {
    let tz = chrono::FixedOffset::east_opt(7 * 3600).unwrap();
    assert_eq!(parse_time("2026-04-16T11:34:59.427116", tz).as_deref(), Some("2026-04-16T04:34:59.427Z"));
    assert_eq!(parse_time("2026-04-16T11:34:59Z", tz).as_deref(), Some("2026-04-16T11:34:59.000Z"));
    assert_eq!(parse_time("2026-04-16T11:34:59+02:00", tz).as_deref(), Some("2026-04-16T09:34:59.000Z"));
    assert_eq!(parse_time("2026-04-16", tz).as_deref(), Some("2026-04-15T17:00:00.000Z"));
    assert_eq!(parse_time("yesterday", tz), None);
    assert_eq!(parse_time("", tz), None);
}

#[test]
fn survey_assigns_unique_ords_and_collapses_identical_chunks_in_one_source() {
    /* source A: chunk_index 0 "a", 1 "b", 1 "c"(different text, same index), 2 "a"(same text as index 0), none "d"
       expected: ords {0→0, "b"→1, "c"→3 (max claimed 2, +1), "d"→4}; the second "a" is collapsed into the first; every ord unique */
}

#[test]
fn oversize_and_odd_text_is_split_or_skipped_never_an_error() {
    /* 130 KB knowledge text → Memories with 3 parts, ids "<id>", "<id>#2", "<id>#3", each ≤ 60_000 bytes, titles end "(part 2/3)";
       40 KB chunk → 2 chunks, second ord = ord + 10_000_000;
       "   \n\t" → Skipped("empty text"); "a\0b" → content "ab";
       decomposed sara am "น\u{0e4d}\u{0e32}" → NFC "นำ" */
}

#[test]
fn since_limits_the_survey_and_a_bad_date_is_the_callers_error() { /* survey(..., Some("2026-03-01")) counts only newer drawers */ }
```

- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-core import::plan -j 4`. Expected: FAIL.
- [ ] **Step 3: Implement** the rules. `local_offset()` = `*chrono::Local::now().offset()` converted with `FixedOffset` (the crate already enables chrono's `clock`). Splitting helper `split_utf8(text: &str, max: usize, prefer: &[&str]) -> Vec<String>` is unit-tested on multi-byte (Thai) text: no part exceeds `max` bytes, parts concatenate back to the input when separators are kept, and no part cuts inside a character.
- [ ] **Step 4: Run the tests** — PASS; `nice -n 15 cargo test -p alice-core -j 4` all green.
- [ ] **Step 5: Commit** — `git add crates/alice-core/src/import/plan.rs crates/alice-core/src/import/mod.rs && git commit -m "feat: classify and map MemPalace drawers into Alice tiers"`

---

### Task 6: Knowledge graph, predicates and tunnels

**Files:**
- Create: `crates/alice-core/src/import/kg.rs`
- Modify: `crates/alice-core/src/import/mod.rs` (`pub mod kg;`), `crates/alice-core/src/import/fixture.rs` (`build_kg`, `build_tunnels`)
- Test: `crates/alice-core/src/import/kg.rs` (`mod tests`)

**Interfaces:**
- Consumes: `store::kg::{AssertFact, FactAction, ObjectKind, normalize_predicate}`, `judge::text::predicate_similarity`, `store::reviews::{NewReview, ReviewType}`, `plan::parse_time` (Task 5).
- Produces:

```rust
pub struct KgTriple { pub id: String, pub subject: String, pub predicate: String, pub object: String, pub valid_from: Option<String>, pub valid_to: Option<String>, pub source_drawer_id: Option<String> }
pub struct Kg { pub triples: Vec<KgTriple>, pub warnings: Vec<String> }
/// `<palace>/knowledge_graph.sqlite3`, else `<mempalace_home>/knowledge_graph.sqlite3`; None when neither exists.
pub fn locate_kg(palace: &Path, mempalace_home: &Path) -> Option<PathBuf>;
pub fn read_kg(path: &Path) -> Result<Kg>;            // opens read-only; entity ids resolved to names
pub struct PlannedFact { pub legacy_id: String, pub add: AssertFact, pub end_at: Option<String>, pub source_drawer_id: Option<String> }
pub struct KgPlan { pub facts: Vec<PlannedFact>, pub reviews: Vec<NewReview>, pub warnings: Vec<String> }
pub fn plan_kg(kg: &Kg, existing_predicates: &[String], assume: FixedOffset) -> KgPlan;

pub struct Tunnel { pub id: Option<String>, pub label: Option<String>, pub source_wing: String, pub source_room: String, pub target_wing: String, pub target_room: String, pub source_drawer_id: Option<String>, pub target_drawer_id: Option<String> }
pub fn read_tunnels(palace: &Path, mempalace_home: &Path) -> Vec<Tunnel>;   // missing/corrupt file → empty, never an error
```

**Rules:** The KG snapshot (taken in Task 8 by the same backup call) is read with `SELECT t.id, s.name, t.predicate, o.name, t.valid_from, t.valid_to, t.source_drawer_id, t.extracted_at FROM triples t LEFT JOIN entities s ON s.id = t.subject LEFT JOIN entities o ON o.id = t.object ORDER BY t.id` (an old palace without `source_drawer_id` is handled by checking `PRAGMA table_info(triples)`; a triple whose entity row is missing uses the entity id as its name and adds a warning). `plan_kg`: sort by `(subject, predicate, valid_from, id)`; each triple becomes `AssertFact { subject, predicate, object, object_kind: ObjectKind::Auto, valid_from: parse_time(valid_from or extracted_at), action: Add, source_memory_id: None, author: Author{agent: Some("mempalace-import")} }`; when `valid_to` parses, `end_at = Some(parse_time(valid_to))` (applied as an `End` action at that time, after the add). `legacy_id` = `"kg:<triple id>"`. Predicate review items: over the distinct `normalize_predicate` values of the KG plus `existing_predicates`, every unordered pair with `predicate_similarity ≥ 0.9` and different normal form yields `NewReview { kind: Predicate, payload: {"predicates":[a,b],"similarity":s,"source":"mempalace import"}, confidence: Some(s), dedupe_key: Some("predicate:<a>|<b>") }` (a < b). **Never** merge automatically (spec §9; the handoff notes predicate similarity is direction-blind — `depends_on` vs `depended_on_by` — which is exactly why this is only a review item). More than 5,000 distinct predicates skip the pairing with a warning. Tunnel file: `<palace>/tunnels.json`, else `<mempalace_home>/tunnels.json`; accepts a JSON array or `{"tunnels": [...]}`; each element tolerant of missing keys (an element without a wing/room pair is dropped with a count the caller reports).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn triples_read_with_entity_names_and_missing_entities_are_warned_not_fatal() { /* build_kg with 3 entities, 3 triples (one subject id missing from entities) → read_kg: names resolved; the orphan uses the id, warnings.len()==1 */ }

#[test]
fn an_ended_triple_plans_an_add_and_an_end_time() { /* valid_to set → end_at Some, add.valid_from Some; the other has end_at None; legacy ids "kg:<id>" */ }

#[test]
fn similar_predicates_become_review_items_never_merges() {
    /* predicates has_bug, had_bug, had_critical_bug, located_at → pairs: has_bug|had_bug (and maybe had_bug|had_critical_bug if ≥0.9);
       located_at pairs with none; every review has kind Predicate and a dedupe key "predicate:a|b" with a<b; plan.facts keep the ORIGINAL predicate text */
}

#[test]
fn an_old_palace_kg_without_source_drawer_id_still_reads() { /* build_kg(legacy_schema=true) */ }

#[test]
fn tunnels_parse_both_shapes_and_survive_garbage() {
    /* array form, {"tunnels":[..]} form, a corrupt file, a missing file → Vec (possibly empty), never Err/panic */
}

#[test]
fn locate_prefers_the_palace_folder_over_the_mempalace_home() { /* both exist → palace one; only home → home; neither → None */ }
```

- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-core import::kg -j 4`. Expected: FAIL.
- [ ] **Step 3: Implement** per the rules. `build_kg` writes the two tables with the exact DDL shown in `mempalace/knowledge_graph.py` (`entities(id, name, type, properties, created_at)`, `triples(id, subject, predicate, object, valid_from, valid_to, confidence, source_closet, source_file, source_drawer_id, adapter_name, extracted_at)`).
- [ ] **Step 4: Run the tests** — PASS.
- [ ] **Step 5: Commit** — `git add crates/alice-core/src/import/kg.rs crates/alice-core/src/import/fixture.rs crates/alice-core/src/import/mod.rs && git commit -m "feat: map the MemPalace knowledge graph and tunnels"`

---

### Task 7: `WriteOp::ImportBatch`

**Why:** one bounded, atomic, idempotent write that the importer (Task 8) and nothing else uses. It keeps the writer's promise (short transactions) and gives resume for free: every legacy id is recorded with the data in the same transaction, so a batch is either fully there or fully absent.

**Files:**
- Modify: `crates/alice-core/src/store/import.rs` (add `ImportBatch`, `apply_batch`), `crates/alice-daemon/src/writer.rs` (`WriteOp::ImportBatch`, `WriteOutcome::Imported`, `import_batch`), `crates/alice-daemon/src/service.rs` (a thin `pub(crate) async fn submit_import(&self, batch) -> Result<BatchOutcome>`; no logic)
- Test: `crates/alice-core/src/store/import.rs` (apply_batch against an in-memory DB), `crates/alice-daemon/src/writer.rs` (`mod tests`: the op through a real writer thread, like `blocking_submit_works_outside_the_runtime`)

**Interfaces:**
- Consumes: Task 1 (`legacy_put`, `legacy_existing`, placeholder projects), Task 5 (`PlannedMemory`/`PlannedChunk` fields), Task 6 (`PlannedFact`, `NewReview`), `store::{memories::prepare/insert, sources::ingest, kg::assert_fact, reviews::create, projects::{upsert_project, add_alias, resolve_slug}}`.
- Produces:

```rust
// alice_core::store::import
pub struct ImportProject { pub slug: String, pub aliases: Vec<String> }       // created with key "import:<slug>" if absent; "global" is the existing global project
pub struct ImportMemory { pub project_slug: String, pub input: NewMemoryInput }   // input.legacy_id and input.created_at are set
pub struct ImportSource { pub project_slug: String, pub source: SourceInput, pub chunks: Vec<ChunkInput> } // source.source_type is Session or File; chunk.legacy_id set
pub struct ImportFact { pub legacy_id: String, pub add: AssertFact, pub end_at: Option<String>, pub source_drawer_id: Option<String> }
pub struct ImportBatch {
    pub projects: Vec<ImportProject>, pub memories: Vec<ImportMemory>, pub sources: Vec<ImportSource>,
    pub facts: Vec<ImportFact>, pub links: Vec<(String, String)>,        // (from drawer id, to drawer id) → relates_to
    pub collapsed: Vec<(String, String)>,                                // (collapsed drawer id, keeper drawer id) → legacy_ids row pointing where the keeper went
    pub reviews: Vec<NewReview>, pub job_id: String,
}
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchOutcome {
    pub memories_new: usize, pub memories_duplicate: usize,
    pub chunks_inserted: usize, pub chunks_skipped: usize, pub sources: usize,
    pub facts_added: usize, pub facts_ended: usize, pub facts_skipped: usize,
    pub links_made: usize, pub links_dropped: usize, pub reviews_created: usize,
    pub collapsed_recorded: usize, pub projects_created: usize,
    pub deferred_collapsed: Vec<(String, String)>,   // pairs whose keeper is not stored yet (the importer sends collapsed pairs last, so normally empty)
    pub warnings: Vec<String>,                        // e.g. an alias that already points at another project
}
pub fn apply_batch(conn: &Connection, b: &ImportBatch, hlc: &mut dyn FnMut() -> String) -> Result<BatchOutcome>;
// alice_daemon::writer
WriteOp::ImportBatch(Box<ImportBatch>)   →   WriteOutcome::Imported(BatchOutcome)
```

**Semantics (pinned by tests):** inside the caller's transaction: (1) ensure each project (`upsert_project` with `ProjectRef { key: "import:<slug>", slug, name: slug, root: None, remote: None }`, skip for `global`) and each alias (`add_alias`; an alias that already points elsewhere is skipped, not an error, and counted in a warning list on the outcome); (2) each memory: if its `legacy_id` is already in `legacy_ids`, skip (counts as duplicate, nothing written); else `prepare` + `insert` — `InsertOutcome::New` records `legacy_put(legacy, "memory", id)`, then sets `judge_state = 'skipped'` on the row; `Duplicate{id}` records `legacy_put(legacy, "memory", existing_id)` (the survivor) and counts `memories_duplicate`; (3) each source: `sources::ingest`, then `legacy_put(chunk legacy_id, "chunk", chunk id)` for every chunk whose legacy id is new (chunk ids are read back with `SELECT id FROM chunks WHERE source_id=? AND ord=?`); chunks whose legacy id is already stored are removed from the list **before** ingest (incremental run); (4) each fact: skip if `legacy_ids` has `legacy_id`; else `kg::assert_fact(add)`, then if `end_at` an `End` action at that time; `source_drawer_id` resolved through `legacy_get` to a memory id → `source_memory_id` (unresolved → none); `legacy_put(legacy, "relation", relation_id)`; (5) links: both drawer ids must resolve to `memory` targets and differ, else `links_dropped`; insert `links(from_id,to_id,type='relates_to',created_by='import',created_at)` with INSERT OR IGNORE; (6) `collapsed`: `legacy_put(collapsed_id, target_type, target_id)` where the target is whatever the keeper's `legacy_ids` row says (a keeper not yet stored → the pair is returned in `deferred_collapsed` so the caller sends it again later; the importer sends collapsed pairs in its last phase, so this is a safety net); (7) reviews via `reviews::create` (dedupe key makes re-runs idempotent). The writer then records **one** `import.batch` event (`Actor::Import`, `target_type "import"`, `target_id = job_id`, `after = {outcome counts}`, never any text), sets `st.inserted = true` when anything was inserted so the embedding worker wakes.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_batch_creates_projects_aliases_memories_and_chunks_in_one_go() {
    let conn = fresh();
    let b = sample_batch(); // 1 project "pg-ai" with alias "wing-pg-ai", 2 memories (one decision), 1 source with 3 chunks
    let out = apply_batch(&conn, &b, &mut hlc()).unwrap();
    assert_eq!((out.memories_new, out.chunks_inserted, out.sources, out.projects_created), (2, 3, 1, 1));
    let m: (String, String) = conn.query_row("SELECT judge_state, status FROM memories WHERE legacy_id='d1'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(m, ("skipped".into(), "pending".into()));
    assert!(legacy_get(&conn, "d1").unwrap().is_some() && legacy_get(&conn, "c2").unwrap().is_some());
    assert_eq!(projects::resolve_slug(&conn, "wing_pg_ai").unwrap().unwrap().slug, "pg-ai");
}

#[test]
fn applying_the_same_batch_twice_changes_nothing() {
    let conn = fresh(); let b = sample_batch();
    apply_batch(&conn, &b, &mut hlc()).unwrap();
    let again = apply_batch(&conn, &b, &mut hlc()).unwrap();
    assert_eq!((again.memories_new, again.chunks_inserted, again.facts_added, again.reviews_created), (0, 0, 0, 0));
    assert_eq!(count(&conn, "memories"), 2); assert_eq!(count(&conn, "chunks"), 3);
}

#[test]
fn identical_memories_collapse_and_both_old_ids_resolve_to_the_survivor() {
    /* two ImportMemory with different legacy ids, same project/kind/title/body → memories_new 1, memories_duplicate 1;
       legacy_get(second) → target_id == first's memory id; exactly one memories row */
}

#[test]
fn facts_keep_validity_and_resolve_their_source_drawer() {
    /* fact with end_at and source_drawer_id "d1" (imported in the same batch): relation has valid_to set and source_memory_id == d1's memory id; legacy "kg:t1" → relation */
}

#[test]
fn links_need_both_ends_and_unresolved_ones_are_counted() { /* d1–d2 linked once; d1–"gone" dropped; links_dropped == 1; relates_to row has created_by 'import' */ }

#[test]
fn review_items_are_deduplicated_across_reruns() { /* same predicate review twice → one open item */ }

#[test]
fn a_failing_row_rolls_the_whole_batch_back() {
    /* a memory with 70_000-byte content makes prepare fail → Err, and nothing from the batch (project, other memories, legacy ids) is stored.
       NOTE: the importer (Task 8) pre-splits such text, so this is the safety-net behaviour */
}
```
and in `writer.rs` tests:

```rust
#[test]
fn the_import_op_goes_through_the_writer_records_one_event_and_wakes_embedding() {
    /* spawn_writer; submit_blocking(WriteOp::ImportBatch(Box::new(sample_batch()))) → Imported(out) with out.memories_new == 2;
       events table: exactly one row with action 'import.batch', actor 'import', and after_json has no memory text (assert it does not contain the body string);
       the wake channel received a message */
}
#[test]
fn an_import_batch_is_refused_while_the_database_is_read_only() { /* read_only.set(..) → Err(ReadOnly) */ }
```

- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-core store::import -j 4` and `… -p alice-daemon writer:: -j 4`. Expected: FAIL.
- [ ] **Step 3: Implement.** `apply_batch` runs inside the transaction the writer already opened per job (read `writer.rs` `run`/`apply` to see how a `WriteOp` is wrapped; every existing op relies on that, so `apply_batch` does not open its own transaction). Keep `ImportBatch` serde-free except `BatchOutcome` (it is returned over HTTP in Task 8). Add `WriteOutcome::name()` arm `"Imported"`.
- [ ] **Step 4: Run the tests** — PASS, plus the whole `alice-daemon` lib tests (`env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --lib -j 4`).
- [ ] **Step 5: Commit** — `git add crates/alice-core/src/store/import.rs crates/alice-daemon/src/writer.rs crates/alice-daemon/src/service.rs && git commit -m "feat: one bounded writer op for importing records"`

### Task 8: The import job

**Why:** this is where the pieces become `alice import mempalace`. The job runs on its own thread (spec §2.1: "Workers: … import"), never on the writer or an async runtime thread; it hands the writer bounded batches; it is the only place that redacts imported text.

**Files:**
- Create: `crates/alice-core/src/import/report.rs`, `crates/alice-daemon/src/importer.rs`, `crates/alice-daemon/tests/import_flow.rs`
- Modify: `crates/alice-core/src/import/mod.rs` (`ImportRequest`, `ImportState`, `ImportStatus`, `locate_palace`, `pub mod report;`), `crates/alice-daemon/src/lib.rs` (`pub mod importer;`), `crates/alice-daemon/src/service.rs` (`start_import`, `import_status`, `import_redactor`, compat auto-enable), `crates/alice-daemon/src/http.rs` (two routes), `crates/alice-daemon/src/client.rs` (`DaemonClient::start_import`, `import_status`)
- Test: `crates/alice-core/src/import/report.rs` (rendering), `crates/alice-daemon/tests/import_flow.rs`

**Interfaces:**
- Consumes: everything from Tasks 1–7; `Service::redactors.for_root(None)` / `Redactor::check(field, text) -> Result<String>` (read `redaction.rs` and `service.rs::redact_field`); `WriterHandle::submit_blocking`; `ReaderPool::with` (for `legacy_existing`, existing predicates); `Service::patch_settings` and `SettingsView.sources` (read `service.rs` around `patch_settings`).
- Produces:

```rust
// alice_core::import
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ImportRequest {
    #[serde(default)] pub palace: Option<String>,        // folder; default: MEMPALACE_PALACE_PATH, then ~/.mempalace/config.json palace_path
    #[serde(default)] pub dry_run: bool,
    #[serde(default)] pub since: Option<String>,         // YYYY-MM-DD or RFC 3339
    #[serde(default)] pub map_wing: std::collections::BTreeMap<String, String>,
    #[serde(default)] pub collection: Option<String>,    // default mempalace_drawers (or config.json collection_name)
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum ImportState { Idle, Running, Done, Failed }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ImportStatus {
    pub state: ImportState, pub job_id: Option<String>, pub phase: String,
    pub done: u64, pub total: u64,
    pub report: Option<ImportReport>, pub error: Option<String>,
}
pub fn locate_palace(arg: Option<&str>, env_palace: Option<&str>, mempalace_home: &Path) -> Result<PathBuf>;
// alice_core::import::report — all fields below are `pub`, Serialize + Deserialize + JsonSchema
pub struct ImportReport { job_id, dry_run, started_at, finished_at: Option<String>, palace: PalaceSummary,
    assumed_utc_offset: String, tiers: TierReport, projects: BTreeMap<String, ProjectCounts>,
    wing_map: BTreeMap<String, WingMapEntry>, duplicates: DuplicateCounts, facts: FactCounts, tunnels: TunnelCounts,
    drawers_seen: u64, drawers_accounted: u64, review_items: u64, already_imported: u64, skipped: BTreeMap<String, u64>, split: SplitCounts,
    blocked_by_redaction: BTreeMap<String, u64>, redacted_fields: u64, warnings: Vec<String>, errors: Vec<String> }
impl ImportReport { pub fn render_text(&self) -> String; }
// alice_daemon::service
impl Service {
    pub fn start_import(self: &Arc<Self>, req: ImportRequest) -> Result<ImportStatus>;   // Err(Invalid) if one is running, the date is bad, or the path is not a palace (checked synchronously before the thread starts)
    pub fn import_status(&self) -> ImportStatus;
}
// HTTP: POST /api/v1/import/mempalace (body ImportRequest) -> 202 ImportStatus; GET /api/v1/import/status -> ImportStatus
```

**Job algorithm (phases are the `phase` strings, in order):**
1. `snapshot` — `locate_palace`; `snapshot(palace, <ALICE_HOME>/import-tmp/<job>/)` (private dir, removed by a drop guard even on failure); KG and tunnels located per Task 6 (palace folder first, `~/.mempalace` second; the second is read only, never copied into the palace dir).
2. `survey` — `reader.wing_counts()`, `WingMap::build(counts, req.map_wing)`, `plan::survey(...)`; the machine's UTC offset (`import::local_offset()`) is recorded in the report.
3. `knowledge` — walk `survey.knowledge_order` in groups of 100: `reader.get(ids)` → `Planner::plan` → redact → `legacy_existing` filter → batches. Earliest-filed first, so the earliest of two exact duplicates is the survivor.
4. `raw` — page the reader by id (2,000 per page); for each raw-tier drawer not already in `legacy_ids`: plan (a `Collapsed` result goes to the `collapsed` list, sent in phase 6), redact, group by `(project, source uri)`, batch.
5. `graph` — `plan_kg` (existing predicates read from `predicates`), tunnel links (both drawer ids), facts, reviews, batched like the rest.
6. `collapsed` — the collapsed-pairs list in batches of 500 (every keeper is stored by now).
7. `done` — compute `drawers_accounted` (imported memories + imported chunks incl. split parts counted once per drawer + collapsed + skipped + blocked + already imported); if it differs from `drawers_seen`, push an `errors` entry naming both numbers; one `sync_state` row `import.mempalace.last` = `{job_id, palace, finished_at, drawers, ok}` (informational: the `legacy_ids` rows written inside each batch's transaction ARE the per-batch checkpoints; a re-run recomputes the plan and skips everything already stored); enable `compat.mempalace` if (and only if) it is at its default (`SettingsView.sources["compat.mempalace"] == Source::Default`), via the same patch path the CLI uses, attributed to `Actor::Import`; delete the snapshot.
- **Batching:** a batch closes at 200 records or 64 KB of text, whichever comes first; one `submit_blocking` per batch, so the writer's group commit interleaves `remember`s between batches.
- **Redaction:** the `Redactor` is `redactors.for_root(None)`; every text field (memory content and title, topics, chunk text) goes through `check(field, text)`; `Err(Refused { detectors, .. })` skips the drawer (counted in `blocked_by_redaction[detector]`, nothing of the text kept) and the import continues; `out != text` counts `redacted_fields`. Redaction runs **after** splitting and **before** `legacy_existing`/batching, on the final strings the writer will store.
- **Dry run:** every step runs except the writer submissions and the `sync_state`/settings writes; counts come from the plan and the `legacy_existing` check, so a dry run reports what a real run would add; it does create (and remove) the snapshot.
- **Failure:** any error before the first batch → state `Failed`, nothing written. An error or `ReadOnly` mid-way → `Failed` with the sentence and "run `alice import mempalace` again to continue; what was imported is kept"; the thread never panics the daemon (`catch_unwind`, as the embedding worker does).
- **Progress:** `done`/`total` count drawers (survey total); `import_status()` is lock-cheap (an `Arc<Mutex<ImportStatus>>`).
- **Shutdown:** the loop checks a shutdown flag between batches (Service already has one for workers; reuse it) and ends `Failed("daemon stopped; run the import again to continue")`.

- [ ] **Step 1: Write the failing tests.** `import_flow.rs` starts a `Service` in a temp `ALICE_HOME` with the fake embedder, the way `tests/http_api.rs` does (copy its helper; do not invent a new bootstrapping path). Fixture palace (built with `alice_core::import::fixture`): wings `pg-ai`, `pg_ai`, `wing_pg_ai`, `devspace`, `sessions`, `wing_claude_code`; knowledge: one `decisions` drawer, one `diary` drawer (agent `claude-code`), one agent-wing drawer mentioning `pg-ai`; two transcripts (`/h/.claude/projects/-Users-me-Code-pg-ai/s1.jsonl`, `…-Code-devspace/s2.jsonl`) with 40 chunks each, one chunk repeated in the same file, one chunk holding `concat!("gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789")`; 30 `devspace` file chunks; KG with 4 triples (one ended, `has_bug`/`had_bug`); a `tunnels.json` with one resolvable and one unresolvable tunnel.

```rust
#[tokio::test]
async fn a_palace_imports_into_both_tiers_with_projects_topics_and_an_honest_report() {
    let f = Flow::new().await;                       // service + fixture palace + palace_hash
    let status = f.run_import(ImportRequest { palace: Some(f.palace_str()), ..Default::default() }).await;
    assert_eq!(status.state, ImportState::Done, "{:?}", status.error);
    let r = status.report.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.drawers_seen, r.drawers_accounted, "every drawer is imported, collapsed, skipped, blocked or already imported");
    assert_eq!((r.tiers.knowledge, r.duplicates.raw), (3, 1));
    assert_eq!(f.project_slugs().await, vec!["devspace", "global", "pg-ai"]);   // pg_ai / wing_pg_ai folded into pg-ai
    assert_eq!(r.wing_map["wing_pg_ai"].to, "pg-ai");
    assert_eq!(r.wing_map["wing_claude_code"].via, "inferred");
    // knowledge is searchable at once, raw only with include_raw, and the diary kept its agent
    assert!(f.recall("decision about the v4 frame", false).await.contains("v4 frame"));
    assert!(f.recall("quickstart transcript phrase", true).await.contains("transcript phrase"));
    assert_eq!(f.diary_author().await.as_deref(), Some("claude-code"));
    assert_eq!(f.judge_states().await, vec!["skipped"]);                          // the judge never sees imports
    assert_eq!(f.facts("alice").await.len(), 4);                                  // 1 ended
    assert_eq!(r.review_items, 1);                                                // has_bug / had_bug
    assert_eq!((r.tunnels.linked, r.tunnels.dropped), (1, 1));
}

#[tokio::test]
async fn secrets_in_imported_history_never_reach_the_database_the_report_or_the_checkpoint() {
    let secret = concat!("gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789");
    /* import; then for table in chunks, memories, events, sync_state, review_items: no row contains the secret; the report JSON and render_text() do not contain it;
       the chunk is present with "[REDACTED:github_token]" (use the detector name the redactor reports); r.redacted_fields >= 1 */
}

#[tokio::test]
async fn block_mode_skips_the_drawer_names_the_detector_and_keeps_going() {
    /* set privacy.redact_mode = block through the settings API first; import → Done, no errors; r.blocked_by_redaction has one detector key with count 1;
       the other chunks are imported; the secret text is nowhere */
}

#[tokio::test]
async fn running_again_imports_nothing_and_new_drawers_are_picked_up() {
    /* second run: tiers all 0, already_imported == first-run total, tables' row counts unchanged;
       add 2 drawers to the source palace (build another fixture round over the same dir) → third run imports exactly 2 */
}

#[tokio::test]
async fn a_dry_run_writes_nothing_and_predicts_the_real_run() {
    /* row counts of every table equal before/after the dry run (including events and sync_state); settings unchanged (compat.mempalace still false);
       dry-run report counts == the real run's counts for tiers, projects, duplicates, skipped */
}

#[tokio::test]
async fn the_source_palace_is_untouched_and_a_bad_palace_is_one_clear_sentence() {
    /* hash of every file in the palace dir before == after (also after a failed run); palace: "/nonexistent" → start_import returns Err(Invalid) containing "does not exist" and "--palace";
       a folder with a text file as sqlite_exact.sqlite3 → Failed or Err mentioning "damaged"; since: Some("last tuesday") → Err before any file is created in import-tmp */
}

#[tokio::test]
async fn a_second_start_while_one_runs_is_refused_and_compat_turns_on_by_itself() {
    /* start a big-ish fixture import, immediately start_import again → Err(Invalid) "an import is already running"; wait Done; settings.compat.mempalace == true and
       the event log has a settings.change with actor "import"; a user who set compat.mempalace=false explicitly (source File) stays false after a re-import */
}

#[tokio::test]
async fn odd_drawers_are_counted_not_fatal() {
    /* empty text, whitespace, NUL, 130 KB knowledge text, 40 KB chunk, missing wing, missing chunk_index, "---" wing → Done with errors empty;
       r.skipped["empty text"] == 2; r.split.memories == 1 (3 parts); r.split.chunks == 1; nothing over the limits in the DB (SELECT max(length(body)) FROM memories <= 65536) */
}
```
and a unit test in `report.rs`:

```rust
#[test]
fn the_text_report_names_every_count_and_never_prints_blocked_text() {
    let r = sample_report();   // all fields filled
    let t = r.render_text();
    for needle in ["Knowledge memories", "Raw session chunks", "Duplicates collapsed", "Already imported", "Wing → project map", "pg_ai → pg-ai", "Skipped", "Redaction"] { assert!(t.contains(needle), "{needle}\n{t}"); }
    assert!(!t.contains("ghp_"));
}
```

- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --test import_flow -j 4`. Expected: FAIL (no `start_import`).
- [ ] **Step 3: Implement** `report.rs`, `importer.rs`, the `Service` methods, the two routes and the client calls. The report text layout (the CLI prints it verbatim in Task 9):

```
MemPalace import — /path/to/palace (sqlite_exact, 119,179 drawers) [dry run]
  Knowledge memories ...... 5,058
  Raw session chunks ...... 76,126 in 130 transcripts
  Raw file chunks ......... 38,000
  Duplicates collapsed .... knowledge 5, raw 27
  Already imported ........ 0
  Knowledge graph ......... 159 facts (2 ended), 4 predicate reviews
  Tunnels ................. 3 linked, 1 dropped (no drawer ids)
  Skipped ................. 24 (MemPalace registry entry: 24)
  Split ................... 0 memories, 0 chunks (over the size limits)
  Redaction ............... 311 fields redacted, 2 drawers blocked (github_token: 2)
  Projects: pg-ai 35,201 · games-platform 18,000 · … (N total)
  Wing → project map (302 wings):
    pg_ai → pg-ai (alias) · wing_pg_ai → pg-ai (alias) · wing_claude_opus_4.7 → inferred per drawer, agent claude-opus-4-7 · …
  Times assumed in UTC+07:00. Embeddings are computed in the background; search works now.
  Warnings: … / Errors: none
```
  `locate_palace`: argument, else `MEMPALACE_PALACE_PATH`, else `<mempalace_home>/config.json` `palace_path`, else error "no palace found; pass --palace <folder>". The daemon passes `env_palace` from its own environment only when started by the CLI that set it — it is safer to resolve in the CLI (Task 9) and always send an absolute `palace`; the daemon-side fallback exists for HTTP callers.
- [ ] **Step 4: Run the tests** — PASS; then `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon -j 4` all green; `pgrep -fl "alice daemon run"` empty.
- [ ] **Step 5: Commit**

```bash
git add crates/alice-core/src/import crates/alice-daemon/src/importer.rs crates/alice-daemon/src/lib.rs crates/alice-daemon/src/service.rs crates/alice-daemon/src/http.rs crates/alice-daemon/src/client.rs crates/alice-daemon/tests/import_flow.rs
git commit -m "feat: the MemPalace import job, report and HTTP routes"
```

---

### Task 9: `alice import mempalace`

**Files:**
- Modify: `crates/alice/src/main.rs` (clap subcommand `import`), `crates/alice/src/cmd.rs` (`import_mempalace`, `import_status`)
- Test: `crates/alice/tests/cli.rs` (golden/snapshot tests in the file's existing style)

**Interfaces:**
- Consumes: `DaemonClient::start_import`/`import_status` (Task 8), `import::locate_palace` (Task 8), the CLI's existing "connect or start the daemon" helper (read `cmd.rs` for how `recall` does it).
- Produces: `alice import mempalace [--palace <path>] [--dry-run] [--since <date>] [--map-wing <wing>=<project>]... [--collection <name>] [--json]` and `alice import status [--json]`.

Behaviour: resolve `--palace` to an absolute path on the CLI side (`MEMPALACE_PALACE_PATH` → `~/.mempalace/config.json` `palace_path` as fallbacks, read-only); `--map-wing` splits on the first `=`; start the job; poll `GET /import/status` every 500 ms; on a TTY print one rewritten progress line on stderr (`survey 12,000/119,179`), otherwise a line per phase change; print the report with `render_text()` (or the JSON with `--json`) on stdout. Exit code 0 only when the state is `done` and `report.errors` is empty; 1 otherwise, with the failure sentence on stderr. Ctrl-C stops waiting only (the daemon continues; `alice import status` shows it; the next `alice import mempalace` resumes). A second import while one runs prints the daemon's "an import is already running" sentence and the current progress.

- [ ] **Step 1: Write the failing tests** (the file's helpers start a throwaway daemon with `ALICE_EMBEDDER=fake`; read how the existing `remember`/`recall` golden tests do it)

```rust
#[test]
fn import_dry_run_prints_the_report_and_changes_nothing() {
    let h = Harness::new();                          // temp ALICE_HOME, fake embedder
    let palace = h.fixture_palace();                  // alice_core::import::fixture (dev-dependency with feature "fixtures")
    let out = h.alice(&["import", "mempalace", "--dry-run", "--palace", palace.to_str().unwrap()]);
    assert!(out.status.success(), "{}", out.stderr);
    for needle in ["MemPalace import", "[dry run]", "Knowledge memories", "Wing → project map"] { assert!(out.stdout.contains(needle), "{needle}\n{}", out.stdout); }
    assert_eq!(h.alice(&["recall", "decision about the v4 frame"]).stdout.matches("v4 frame").count(), 0, "nothing was imported");
}

#[test]
fn import_then_recall_finds_the_old_memory_and_json_is_valid() {
    /* real run with --json: parse stdout as ImportReport; errors empty; then `alice recall "<diary phrase>"` finds it; `alice import status --json` says done; exit codes 0 */
}

#[test]
fn bad_inputs_exit_nonzero_with_one_sentence_on_stderr() {
    /* --palace /nonexistent → status 1, stderr contains "does not exist"; --since nope → stderr contains "--since"; --map-wing nope (no '=') → clap-style usage error, status 2 */
}

#[test]
fn map_wing_overrides_reach_the_daemon() { /* --map-wing wing_project_alice=alice → report.wing_map["wing_project_alice"].to == "alice" (with --json) */ }
```

- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-memory --test cli import -j 4` (package name `alice-memory`; copy the exact `-p` from the existing test invocations in `.github/workflows/ci.yml`). Expected: FAIL.
- [ ] **Step 3: Implement.** Help text for the subcommand states the safety facts in one place: "Reads the palace through a read-only snapshot; MemPalace does not need to be installed or stopped. Run it again any time: it imports only what is new. Add --dry-run to see what would happen."
- [ ] **Step 4: Run the tests** — PASS; `pgrep -fl "alice daemon run"` empty (the harness stops its daemon).
- [ ] **Step 5: Commit** — `git add crates/alice/src/main.rs crates/alice/src/cmd.rs crates/alice/tests/cli.rs && git commit -m "feat: alice import mempalace"`

---

### Task 10: Compat core — `mempalace_*` on the Service

**Why:** spec §8.2: existing prompts and hooks that call `mempalace_*` keep working against Alice. This task is the translation, independent of MCP, so it is testable with plain function calls and reusable by HTTP and the MCP adapter (Task 11).

**Files:**
- Create: `crates/alice-daemon/src/compat.rs`, `crates/alice-core/src/store/compat.rs` (read-only queries: overview counts, recent diaries, drawer resolution), `crates/alice-daemon/tests/compat_flow.rs`
- Modify: `crates/alice-core/src/store/mod.rs` (`pub mod compat;`), `crates/alice-daemon/src/lib.rs`, `crates/alice-daemon/src/service.rs` (`compat_call`), `crates/alice-daemon/src/http.rs` (`POST /api/v1/compat/call`), `crates/alice-daemon/src/client.rs` (`DaemonClient::compat_call`)
- Test: `crates/alice-daemon/tests/compat_flow.rs`

**Interfaces:**
- Consumes: `Service::{recall, remember, context, get, update, facts, assert_fact, projects}`, `store::import::{legacy_get}`, `store::projects::resolve_slug`, `normalize::{slugify, normalize_topic}`, `plan::kind_for_room` (Task 5; the same room → kind rule the importer uses).
- Produces:

```rust
// alice_daemon::compat
pub const SUPPORTED: [&str; 15] = [
  "mempalace_status", "mempalace_search", "mempalace_add_drawer", "mempalace_diary_write", "mempalace_diary_read",
  "mempalace_get_drawer", "mempalace_update_drawer", "mempalace_delete_drawer",
  "mempalace_kg_query", "mempalace_kg_add", "mempalace_kg_invalidate", "mempalace_kg_supersede", "mempalace_kg_timeline",
  "mempalace_list_wings", "mempalace_list_rooms",
];
pub fn is_compat_name(name: &str) -> bool;            // starts with "mempalace_"
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct CompatCall { pub tool: String, #[serde(default)] pub arguments: serde_json::Value, #[serde(default)] pub cwd: Option<String>, #[serde(default)] pub agent: Option<String> }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompatResult { pub value: serde_json::Value }      // always a JSON object; failures are {"error": "..."} like MemPalace's own
impl Service { pub async fn compat_call(&self, call: CompatCall) -> alice_core::Result<CompatResult>; }
// alice_core::store::compat
pub struct Overview { pub memories: u64, pub chunks: u64, pub wings: BTreeMap<String, u64>, pub rooms: BTreeMap<String, u64> }
pub fn overview(conn: &Connection) -> Result<Overview>;   // wings = project slug → memories+chunks; rooms = topic → memories (top 200 by count)
pub fn recent_diaries(conn: &Connection, agent: &str, project: Option<&str>, limit: usize) -> Result<Vec<Memory>>;
pub enum Drawer { Memory(String /*memory id*/), Chunk { id: String, project_slug: String, text: String, source_uri: String, ord: i64, created_at: String } }
pub fn resolve_drawer(conn: &Connection, drawer_id: &str) -> Result<Option<Drawer>>;  // Alice id, legacy id (via legacy_ids and memories.legacy_id), or chunk id
```

**Exact output shapes** come from MemPalace's own source (read-only, installed at `/Users/icue/.venv/lib/python3.13/site-packages/mempalace/mcp_server/`): `tools_read.py` (`tool_status`, `tool_list_wings`, `tool_list_rooms`, `tool_search`), `tools_write.py` (`tool_add_drawer`, `tool_get_drawer`, `tool_update_drawer`, `tool_delete_drawer`), `tools_diary.py`, `tools_kg.py`, and the argument schemas in `schemas.py`. The implementer reads those functions and mirrors **every top-level key MemPalace returns** (keys Alice cannot fill get a neutral value: `0`, `""`, `[]`, `{}`), then adds `"alice": true` to `status` and `"note"` where behaviour differs. Known shapes to start from: `status` → `{total_drawers, wings{}, rooms{}, protocol, aaak_dialect, backend}` (+ `context` text, `wing_project_map`); `list_wings` → `{wings: {name: count}}`; `add_drawer` → `{success, drawer_id, wing, room}` (duplicate: `{success: true, reason: "already_exists", drawer_id}`); `get_drawer` / `diary_*` / `kg_query` as in the source.

**Mapping rules (pinned by tests):**
- *wing → project:* `projects::resolve_slug(wing)` (aliases such as `wing_pg_ai`, `pg_ai` resolve). Unknown wing on a **read** (`search`, `list_rooms`, `diary_read`): empty result plus `"note": "no project or alias named <wing>"`. Unknown wing on a **write** (`add_drawer`, `diary_write`): the write goes to the caller's current project (`cwd`) with `topics = [room]`, and the answer carries `"note": "wing <w> is not an Alice project; saved in <project>"` (agents never create containers, spec §3.2).
- *room → topic:* `normalize_topic(room)`; `add_drawer` kind = `kind_for_room(room)` (Task 5), `title` first line, `author.agent = added_by` else the caller's agent; `source_file` is appended as a final line `Source: <path>` to the body (Alice has no per-memory file field).
- *search:* `RecallRequest { query, project: <wing>, topics: [room], since, before, include_raw: true, format: Json, limit: args.limit (default 5, ≤ 50), max_tokens: 6000 }`; each hit becomes `{text: body-or-snippet, wing: project, room: first topic or "", source_file: source uri or "", similarity: score ÷ best score (3 decimals, 1.0 for the best), drawer_id: id, created_at, matched}`; `max_distance`, `candidate_strategy`, `cli_compatible`, `context` are accepted and ignored (named in a `"note"`). With `room` set, only the knowledge tier can match (chunks carry no topics); the note says so.
- *diary_write:* `remember(kind=diary, content=entry|content, title=format!("Diary: {topic or 'general'}"), topics=[topic], author.agent=agent_name, project=wing or cwd project)`; *diary_read:* `recent_diaries(agent_name, wing, last_n default 10)` → `{agent, entries: [{date, timestamp, topic, content}], total, showing}`.
- *get/update/delete drawer:* `resolve_drawer`. Memory → `get` / `update{action: edit, body, topics}` / `update{action: forget}`. `update_drawer` with `wing` (a move) answers `{"error": "moving a memory to another project is not supported; remember it in the new project and forget this one"}`. A raw chunk: `get_drawer` returns its text, `wing`, `room: ""`, `metadata{source_file, chunk_index}`; update/delete answer `{"error": "raw transcript chunks cannot be changed one by one; …"}`.
- *kg:* `kg_query` → `facts(entity, as_of)`, split into `active_facts` / `historical_facts` (valid_to null vs set); `direction` filters on whether the entity is subject or object; `kg_add` → `assert_fact(add)` then `end` when `valid_to` is given; `kg_invalidate` → `end` at `ended` (default now); `kg_supersede` → `assert_fact(supersede)` with `new_object` at `at` (the old value is closed by Alice; `old_object` is checked against the current value and a mismatch is reported in `"note"` but not refused); `kg_timeline` → `facts(entity, timeline)` with `limit`/`offset` applied.
- *argument coercion:* a missing required argument → `{"error": "missing required argument: query"}`; numbers sent as strings (`"limit": "5"`) and booleans as strings are accepted; non-object `arguments` (a JSON string holding an object, as some clients send) is parsed once; wrong types give `{"error": "argument <name> must be a <type>"}`. Never a panic, never a protocol error.
- *unsupported tools:* any other `mempalace_*` name answers `{"supported": false, "tool": <name>, "message": "<what Alice does instead>"}` using this table, keyed by name: `mempalace_mine`, `mempalace_sync`, `mempalace_delete_by_source`, `mempalace_checkpoint` → "Alice captures sessions through its agent hooks and imports old palaces with `alice import mempalace`; save a memory with `remember`."; `mempalace_check_duplicate` → "`remember` detects exact duplicates itself (status duplicate); use `recall` to look for similar memories."; `mempalace_list_drawers` → "use `recall` with `topics`, `since` and `before`."; `mempalace_get_taxonomy` → "use `mempalace_list_wings` and `mempalace_list_rooms`, or the `context` tool."; `mempalace_kg_stats`, `mempalace_graph_stats`, `mempalace_memories_filed_away` → "the admin `stats` tool (mcp.admin_tools) and `alice status` show counts."; `mempalace_traverse`, `mempalace_find_tunnels`, `mempalace_create_tunnel`, `mempalace_list_tunnels`, `mempalace_delete_tunnel`, `mempalace_follow_tunnels`, `mempalace_list_hallways`, `mempalace_delete_hallway` → "Alice links memories (`remember` with `supersedes`, `update_memory` supersede) and the graph view in the app shows connections; there are no tunnels."; `mempalace_hook_settings` → "use `alice config`."; `mempalace_get_aaak_spec` → "Alice does not use AAAK; memories are plain text."; `mempalace_mesh_peers`, `mempalace_reconnect`, `mempalace_event_*`, `mempalace_task_create`, `mempalace_artifact_*`, `mempalace_patch_submit` → "coordination features are not part of Alice; agents share memory through `remember` and `recall`."; any other `mempalace_*` → "Alice does not provide <name>. Its tools are recall, remember, context, get_memory, update_memory, query_facts and assert_fact.".

- [ ] **Step 1: Write the failing tests** in `compat_flow.rs` (reuse the Task 8 `Flow` fixture: an imported palace plus a helper `call(tool, json!({…}))`):

```rust
#[tokio::test]
async fn old_wing_names_resolve_through_aliases() {
    let f = Flow::imported().await;
    let r = f.call("mempalace_search", json!({"query": "v4 frame", "wing": "wing_pg_ai"})).await;
    assert!(r["results"].as_array().unwrap().iter().any(|x| x["text"].as_str().unwrap().contains("v4 frame")));
    assert!(r["results"][0]["wing"] == "pg-ai" && r["results"][0]["similarity"] == 1.0);
    let w = f.call("mempalace_list_wings", json!({})).await;
    assert!(w["wings"]["pg-ai"].as_u64().unwrap() > 0 && w.get("wing_pg_ai").is_none());
}

#[tokio::test]
async fn add_get_update_delete_round_trip_with_old_style_arguments() {
    /* add_drawer {wing:"pg_ai", room:"decisions", content:"We chose X because Y", added_by:"codex"} → success, drawer_id;
       the memory is kind decision, topics ["decisions"], author codex; get_drawer {drawer_id} → content;
       adding the same again → reason already_exists and the same id; update_drawer {drawer_id, content:"We chose Z"} → get shows Z;
       delete_drawer → get_drawer answers {"error": …} and recall no longer finds it */
}

#[tokio::test]
async fn ids_from_the_old_palace_still_resolve_even_when_they_were_collapsed() {
    /* the fixture's knowledge duplicate: both drawer ids give get_drawer the SAME content; a raw chunk drawer id gives its text with metadata.source_file;
       update_drawer on a raw id → error sentence containing "raw" */
}

#[tokio::test]
async fn diary_write_and_read_round_trip_for_an_agent() { /* diary_write{agent_name:"claude-code", entry:"did X", topic:"setup"} then diary_read{agent_name:"claude-code", last_n:"3"} (string number) → entries newest first, total ≥ 1; another agent sees none of them */ }

#[tokio::test]
async fn the_knowledge_graph_calls_cover_add_query_invalidate_supersede_timeline() {
    /* kg_add alice uses sqlite valid_from 2026-01-01; kg_query alice → active_facts has it; kg_supersede alice uses sqlite→postgres at 2026-06-01 → active has postgres, historical has sqlite;
       kg_invalidate alice uses postgres ended 2026-07-01 → active empty; kg_timeline alice → 2 entries in time order; kg_add with valid_to creates an ended fact */
}

#[tokio::test]
async fn unknown_wings_unsupported_tools_and_bad_arguments_get_helpful_answers() {
    /* search wing:"nope" → results [] + note; add_drawer wing:"nope", cwd=a project → saved in that project + note;
       mempalace_mine → supported:false and message contains "alice import mempalace"; mempalace_totally_new → message lists Alice's tools;
       search without query → {"error":"missing required argument: query"}; search {"query": 42} → error mentions "query"; limit:"abc" → error mentions "limit";
       arguments given as a JSON string '{"query":"x"}' works; none of these returns Err(..) from compat_call */
}

#[tokio::test]
async fn status_gives_counts_and_the_wing_map_and_stays_small() {
    /* status → total_drawers == memories + chunks, wings has "pg-ai", "alice": true, context text present, serialized length < 6000 bytes (the owner's 294-wing status cost tokens every session: Alice's must not) */
}
```

- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --test compat_flow -j 4`. Expected: FAIL.
- [ ] **Step 3: Implement** `store/compat.rs` queries (all read-only, bounded: `overview` runs two `GROUP BY` queries and caps rooms at 200; at 1M chunks the per-project chunk count uses `chunks_project(project_id, created_at)` — index-only count per project, measured in Task 12), then `compat.rs`. Write one table-driven function per tool (`async fn status(svc, args, ctx) -> Value`), a small `Args` wrapper with `required_str`, `opt_str`, `opt_u64`, `opt_bool` that does the coercions above, and the `UNSUPPORTED` table as a `match`. `Service::compat_call` returns `Err` only for infrastructure failures (read-only database on a write tool becomes `{"error": "memory is read-only: …"}`, the same sentence as other tools); everything an agent can cause is a JSON answer.
- [ ] **Step 4: Run the tests** — PASS; `nice -n 15 cargo test -p alice-daemon -j 4` all green.
- [ ] **Step 5: Commit**

```bash
git add crates/alice-core/src/store/compat.rs crates/alice-core/src/store/mod.rs crates/alice-daemon/src/compat.rs crates/alice-daemon/src/lib.rs crates/alice-daemon/src/service.rs crates/alice-daemon/src/http.rs crates/alice-daemon/src/client.rs crates/alice-daemon/tests/compat_flow.rs
git commit -m "feat: MemPalace compatibility layer on the service"
```

---

### Task 11: Compat on MCP — schemas, live gating, stdio shim

**Prerequisite (check first, stop if it fails):** `feat/m2-judge-settings` contains M2 Task 11 (the review inbox and **admin MCP toolset gated by `mcp.admin_tools`, read per call, absent from `list_tools` when off**). Run `git merge --no-edit feat/m2-judge-settings`, then `grep -n "admin_tools" crates/alice-daemon/src/mcp.rs`. If the gate is not there, **stop and tell the controller**: compat must use the same mechanism, and inventing a second one would diverge. (Tasks 1–10 do not depend on it.)

**Files:**
- Create: `crates/alice-daemon/src/compat_schema.rs`
- Modify: `crates/alice-daemon/src/mcp.rs` (list/call routing, `Backend::compat_enabled`, `Backend::compat_call`), `crates/alice-daemon/src/client.rs` (`DaemonClient::compat_enabled` via the settings endpoint, cached 2 s), `crates/alice-daemon/src/lib.rs`
- Test: `crates/alice-daemon/tests/compat_flow.rs` (add MCP tests; reuse the reference-client helper from `tests/mcp.rs`), `crates/alice/tests/cli.rs` (shim test)

**Interfaces:**
- Consumes: Task 10 (`compat_call`, `SUPPORTED`, `is_compat_name`), the gating mechanism of M2 Task 11, `settings.compat.mempalace`.
- Produces: `compat_schema::tools() -> Vec<rmcp::model::Tool>` — one per `SUPPORTED` name with MemPalace's own description wording shortened to one sentence and an `inputSchema` listing exactly MemPalace's property names, types and required arguments (from `schemas.py`, copied by hand into a Rust table; the property lists printed by the planner for the 15 tools are: `search` {query*, limit, wing, room, source_file, since, before, max_distance, candidate_strategy, cli_compatible, context}; `add_drawer` {wing*, room*, content*, source_file, added_by}; `status` {}; `list_wings` {}; `list_rooms` {wing}; `get_drawer` {drawer_id*}; `update_drawer` {drawer_id*, content, wing, room}; `delete_drawer` {drawer_id*}; `diary_write` {agent_name*, entry, topic, wing, content}; `diary_read` {agent_name*, last_n, wing}; `kg_query` {entity*, as_of, direction}; `kg_add` {subject*, predicate*, object*, valid_from, valid_to, source_closet, source_file, source_drawer_id}; `kg_invalidate` {subject*, predicate*, object*, ended}; `kg_supersede` {subject*, predicate*, old_object*, new_object*, at}; `kg_timeline` {entity, limit, offset}; `*` = required — the implementer re-verifies each against `schemas.py`).

**Behaviour:** `list_tools` = the core router's tools (+ admin tools per M2) + `compat_schema::tools()` **only while `compat.mempalace` is true**, evaluated on every call (no restart). `call_tool` for a `mempalace_*` name: enabled → `Backend::compat_call` and the JSON value returned as text content (an `{"error": …}` value is still a normal result, as MemPalace does); disabled → the same "unknown tool" error any unknown name gets, plus the sentence "MemPalace compatibility is off; turn it on with `alice config set compat.mempalace true`" in its data. An enabled-but-unsupported name (`mempalace_mine`) is **not listed** but **is answered** (Task 10's message). The caller's `clientInfo.name` becomes the agent (`CompatCall.agent`), the project root comes from the same `cwd` resolution the core tools use. If rmcp offers `notify_tool_list_changed`, send it to connected peers when the setting flips (subscribe to the settings stream the SSE endpoint already uses); if the API is awkward, per-call evaluation is the required behaviour and the notification is optional — say which in the ledger.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn compat_tools_appear_and_vanish_with_the_setting_without_a_restart() {
    let f = Flow::imported_with_mcp().await;                    // in-process daemon + rmcp reference client over Streamable HTTP
    let names = |t: Vec<String>| t.into_iter().filter(|n| n.starts_with("mempalace_")).count();
    assert_eq!(names(f.mcp_tool_names().await), 0, "off by default on a fresh home");
    f.set_compat(true).await;                                    // settings PATCH, as `alice config set` does
    let on = f.mcp_tool_names().await;
    assert_eq!(names(on.clone()), 15);
    for core in ["recall", "remember", "context", "get_memory", "update_memory", "query_facts", "assert_fact"] { assert!(on.contains(&core.to_string())); }
    f.set_compat(false).await;
    assert_eq!(names(f.mcp_tool_names().await), 0);
}

#[tokio::test]
async fn a_call_while_compat_is_off_is_refused_with_the_way_to_turn_it_on() { /* mempalace_search with compat off → tool error mentioning "compat.mempalace"; with it on → results */ }

#[tokio::test]
async fn unsupported_names_are_answered_but_not_listed() { /* compat on: tool list lacks mempalace_mine; calling it returns the "alice import mempalace" message as a normal result */ }

#[tokio::test]
async fn every_listed_schema_names_mempalaces_arguments() {
    /* for each tool in compat_schema::tools(): inputSchema.type == "object"; required list == the table above; a MemPalace-shaped call built from the schema's own properties succeeds for search/add_drawer/kg_add */
}

#[tokio::test]
async fn author_is_the_mcp_client_name_for_compat_writes() { /* add_drawer without added_by through an MCP client named "codex" → memory.author.agent == "codex"; with added_by:"claude" → "claude" */ }
```
and in `crates/alice/tests/cli.rs`: `the_stdio_shim_lists_compat_tools_when_the_daemon_has_them_on` (start the daemon, `alice config set compat.mempalace true`, speak MCP to `alice mcp` over stdio with the test client the file already has, assert 15 `mempalace_*` names and one successful `mempalace_list_wings` call).

- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --test compat_flow mcp -j 4`. Expected: FAIL.
- [ ] **Step 3: Implement.** Replace the macro-generated `list_tools`/`call_tool` only as far as M2 Task 11 already did for admin tools (extend its code path, do not fork it). Keep the shim start-up budget (< 20 ms): `compat_enabled` for the shim is fetched lazily on the first `list_tools`/`call_tool`, never at start-up.
- [ ] **Step 4: Run the tests** — PASS; `nice -n 15 cargo test -p alice-daemon --test mcp -j 4` (the existing MCP conformance suite) still green; `pgrep -fl "alice daemon run"` empty.
- [ ] **Step 5: Commit** — `git add crates/alice-daemon/src/compat_schema.rs crates/alice-daemon/src/mcp.rs crates/alice-daemon/src/client.rs crates/alice-daemon/src/lib.rs crates/alice-daemon/tests/compat_flow.rs crates/alice/tests/cli.rs && git commit -m "feat: expose the MemPalace tools over MCP while compat is on"`

### Task 12: Concurrency and crash safety during an import

**Why:** the whole reason Alice exists is that MemPalace writers block each other during a mine (spec §0.1 problem 1). Spec §10 makes "20 clients writing and searching while an import runs" and "`kill -9` during import" acceptance tests. Task 7's bounded batches are the mechanism; this task proves it.

**Files:**
- Create: `crates/alice-daemon/tests/import_concurrency.rs`
- Modify: `crates/alice/tests/cli.rs` (the kill test), `docs/benchmarks/m3.md` (create; numbers filled in Task 14)
- Test: both files above

**Interfaces:** Consumes the `Flow`-style bootstrap of Task 8, `alice_core::import::fixture` with a scaled generator `fixture::synthetic_palace(dir, drawers: usize)` (add it here: mostly transcript chunks of 600–900 chars of mixed Thai/English/code words, 3% knowledge drawers, 5% repeats), and the pattern of `crates/alice-daemon/tests/service_concurrency.rs` (read it first: it already measures `remember`/`recall` p95 and counts lost writes; copy its helpers).

- [ ] **Step 1: Write the failing tests**

```rust
// import_concurrency.rs
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_clients_keep_working_while_a_palace_imports() {
    let n = std::env::var("ALICE_TEST_BULK").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000usize);
    let f = Flow::with_synthetic_palace(n).await;                  // fake embedder, temp home
    f.start_import_default();                                      // returns at once; the job runs on its thread
    let stats = f.twenty_clients_for_until_import_done().await;    // 20 tasks: each loops remember(unique text) + recall(random word), recording latency and every acknowledged id
    assert!(f.import_done_ok().await);
    // nothing lost, nothing duplicated
    assert_eq!(f.count_ids_present(&stats.acknowledged).await, stats.acknowledged.len());
    assert_eq!(f.duplicate_memory_hashes().await, 0);
    assert_eq!(f.chunk_count().await + f.collapsed_count().await, f.expected_chunks);
    // latency budgets are enforced in release builds only (same rule as service_concurrency.rs)
    if !cfg!(debug_assertions) {
        assert!(stats.remember_p95_ms < 50.0, "remember p95 {} ms", stats.remember_p95_ms);
        assert!(stats.recall_p95_ms < 200.0, "recall p95 {} ms", stats.recall_p95_ms);
    }
    eprintln!("import {} drawers in {:?}; remember p95 {:.1} ms, recall p95 {:.1} ms", n, f.import_elapsed(), stats.remember_p95_ms, stats.recall_p95_ms);
}

#[tokio::test]
async fn one_batch_never_holds_the_writer_longer_than_the_slice_budget() {
    /* instrument: submit a remember every 5 ms from a task while a 20k import runs; the longest single remember ack (debug build: < 750 ms, release: < 150 ms)
       is the proof that no batch is long; also assert every ImportBatch the importer built had ≤ 200 records and ≤ 64 KB text (importer exposes a test counter `max_batch_records/bytes`) */
}
```
```rust
// crates/alice/tests/cli.rs
#[test]
#[cfg(unix)]
fn kill_minus_nine_during_an_import_loses_nothing_and_the_rerun_finishes_exactly() {
    /* Harness: throwaway ALICE_HOME, ALICE_EMBEDDER=fake, a 30k-drawer synthetic palace.
       1. start `alice import mempalace --palace P --json` in the background; poll `alice import status --json` until done ≥ 3,000 drawers;
       2. send remember("acked before kill N") ×20 and keep the ids; SIGKILL the daemon pid (from daemon.json);
       3. start a daemon again (any `alice` call does), run the import again to the end → exit 0, errors empty;
       4. expected: every acknowledged id is present; `PRAGMA quick_check` = ok (open the db read-only AFTER stopping the daemon);
          memories + chunks + collapsed rows == the fixture's counts exactly (no duplicates, no gaps); a third run reports tiers all 0 and already_imported == total;
          the palace directory hash is unchanged. */
}

#[test]
fn a_palace_on_a_full_or_read_only_target_fails_cleanly_and_resumes() {
    /* make ALICE_HOME/import-tmp unwritable (unix: chmod 0555 on a pre-created import-tmp) → the run ends Failed with a sentence naming the folder and what to do; restore permissions → rerun succeeds. */
}
```

- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --test import_concurrency -j 4` and the cli tests. Expected: FAIL (missing helpers). Then add `synthetic_palace` and the importer test counters (`ImportManager::batch_stats()` returning `{max_records, max_bytes, batches}` behind `#[cfg(any(test, feature = "test-stats"))]` is not needed: a plain `pub fn` on the status is fine).
- [ ] **Step 3: Implement the helpers; fix what the tests find.** If the p95 assertions fail in `--release`, the fix is in the batching (smaller batches, a pause of 2–5 ms between batches while the writer queue is non-empty — `WriterHandle` can expose queue depth) and **not** in relaxing a budget. Record what changed in the ledger.
- [ ] **Step 4: Run the tests** — debug: `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --test import_concurrency -j 4`; release latency proof (one heavy run, alone): `ALICE_TEST_BULK=100000 env -u TYPESAFE_API_KEY nice -n 15 cargo test --release -p alice-daemon --test import_concurrency twenty_clients -j 4 -- --nocapture` → paste the printed line into `docs/benchmarks/m3.md` under "Import under load". Kill test: `nice -n 15 cargo test -p alice-memory --test cli kill_minus_nine -j 4`. `pgrep -fl "alice daemon run"` empty afterwards.
- [ ] **Step 5: Commit** — `git add crates/alice-daemon/tests/import_concurrency.rs crates/alice/tests/cli.rs crates/alice-core/src/import/fixture.rs crates/alice-daemon/src/importer.rs docs/benchmarks/m3.md && git commit -m "test: concurrency and crash safety while importing"`

---

### Task 13: Gold-set search quality — Alice vs MemPalace

**Why:** M3 acceptance: "gold-set search quality beats MemPalace" (spec §12) with the §10 method: nDCG@10 and Recall@10 on a synthetic multilingual fixture that lives in the repo, plus a local-only gold set from the owner's palace that is never committed. Honest comparison needs MemPalace's real answers on the same data, recorded once.

**Files:**
- Create: `scripts/gen_gold_corpus.py` (deterministic generator, `random.Random(20261014)`), `crates/alice-daemon/tests/data/gold_corpus.json`, `scripts/m3_mempalace_baseline.py`, `crates/alice-daemon/tests/data/mempalace_baseline.json`, `crates/alice-daemon/tests/import_quality.rs`
- Modify: `docs/benchmarks/m3.md`
- Test: `crates/alice-daemon/tests/import_quality.rs`

**Corpus (synthetic, no real data):** ~160 drawers and 48 queries. Drawers: 40 English knowledge notes (decisions, bugfix, howto), 25 Thai knowledge notes, 15 mixed Thai/English, 20 identifier/path notes (`HostLinkReconnect`, `writer_lease`, `internal/teacher/jev/client.go`), 35 transcript chunks in 6 files across 3 projects (including one spelled `wing_pg_ai`), 20 code-file chunks, 10 exact/near duplicates, 5 diary entries. Queries (each with `category`, `relevant` = drawer ids, `lexical_ok` = true when a plain word match suffices): 14 `fact` (English), 8 `thai`, 6 `mixed`, 8 `identifier`, 6 `paraphrase` (no shared content words with the answer), 3 `cross_lingual` (Thai question, English answer), 3 `raw_only` (answer exists only in a transcript chunk). **The corpus and the query list are committed first, in their own commit, before either system is run on them.** Nobody edits the corpus after seeing a result (that would be tuning to win); a corpus bug found later is fixed in a separate commit that names the reason, and both systems are re-run.

**Baseline script** (`scripts/m3_mempalace_baseline.py`, local only, needs the owner's installed MemPalace at `/Users/icue/.venv`): builds a **throwaway** palace from `gold_corpus.json` in a temp dir and records MemPalace's top-10 for every query into `mempalace_baseline.json` (`{"mempalace_version": "...", "queries": {"q01": ["drawer_id", …], …}}`). Safety rules the script enforces itself and the task verifies:
1. Run `python -I` with `MEMPALACE_PALACE_PATH=<temp dir>` and drive MemPalace through its **Python API** on that path (not the CLI, which routes writes through the hub): read `mempalace/backends/sqlite_exact.py`, `mempalace/searcher/` and `mcp_server/tools_write.py`/`tools_read.py` for the calls; it must also use `sqlite_exact` so the fixture and the real palace share a backend.
2. Before and after, the script `shasum`s `~/.mempalace/config.json` and `stat`s `/Users/icue/Code/AI/memory_vault/sqlite_exact.sqlite3` **size and `-wal` presence only**, prints both and aborts the run (exit 3) if the config hash changed or if any path under the real palace appears in the process's open files (`lsof -p $$` filtered by that prefix) at the end. (The owner's agents write to the real palace independently, so its size may change; the check is "this process never opened it".)
3. If MemPalace's embedding model is not available offline, **stop and ask the controller** instead of downloading anything.

**Tests** (`import_quality.rs`):

```rust
#[tokio::test]
async fn gold_queries_with_a_plain_word_match_are_found_after_an_import() {          // CI: fake embedder, lexical only
    let f = Flow::from_gold_corpus().await;                                           // build_sqlite_exact from gold_corpus.json, import, wait
    let (hits, total) = f.recall_at_10(|q| q.lexical_ok).await;
    assert!(hits as f64 / total as f64 >= 0.90, "{hits}/{total}");
    assert!(f.score_report().ndcg_at_10 >= 0.80);
}

#[test]
fn the_baseline_file_matches_the_corpus() { /* every baseline query id exists in the corpus; every listed drawer id exists; 48 queries; guards a stale baseline */ }

#[tokio::test]
#[ignore = "real e5 model (linked from ~/.alice/models); ask the controller before running"]
async fn alice_beats_mempalace_on_the_gold_set() {
    let f = Flow::from_gold_corpus_real_model().await;                                // waits for background embedding to finish
    let alice = f.score_all();                                                        // ndcg@10 and recall@10, overall and per category
    let base = MemPalaceBaseline::load();                                             // same metric code on the recorded lists
    eprintln!("{}", comparison_table(&alice, &base));                                 // markdown, pasted into docs/benchmarks/m3.md
    assert!(alice.overall.ndcg_at_10 > base.overall.ndcg_at_10, "{} vs {}", alice.overall.ndcg_at_10, base.overall.ndcg_at_10);
    assert!(alice.overall.recall_at_10 >= base.overall.recall_at_10);
    for c in CATEGORIES { assert!(alice.by_cat[c].ndcg_at_10 + 0.05 >= base.by_cat[c].ndcg_at_10, "{c} regressed"); }
}
```
Metric code: the same `ndcg_at_k`/`recall_at_k` for both sides (reuse `alice_core::judge::metrics::{ndcg_at_k,…}` if its signature fits — read it; otherwise 20 lines here, unit-tested with a hand-computed example). A hit is mapped to its drawer ids through `legacy_ids` (reverse lookup: all legacy ids pointing at the hit's memory or chunk), so a result that is the survivor of a collapsed duplicate counts for every original id.

- [ ] **Step 1:** write `gen_gold_corpus.py`, run it, review the JSON by eye for sanity (languages, ids unique, every `relevant` id exists), **commit corpus + generator alone**: `git add scripts/gen_gold_corpus.py crates/alice-daemon/tests/data/gold_corpus.json && git commit -m "test: synthetic gold corpus for the MemPalace comparison"`.
- [ ] **Step 2:** write the tests above (CI test and baseline-consistency test first); run — Expected: FAIL (no baseline file / helpers).
- [ ] **Step 3:** write `m3_mempalace_baseline.py`; run it **once** under the safety rules; commit the script and `mempalace_baseline.json`. If the script cannot run safely, stop and tell the controller; do not fake a baseline.
- [ ] **Step 4:** implement the helpers; `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --test import_quality -j 4` — PASS for the two CI tests. **Ask the controller for the go** before running the ignored real-model test; when allowed: `… -- --ignored --nocapture alice_beats_mempalace_on_the_gold_set`, one heavy run, alone. Paste the table into `docs/benchmarks/m3.md`. **If Alice does not beat MemPalace, report the numbers as they are and stop: that is a finding for the controller, not something to fix by editing the corpus.**
- [ ] **Step 5: Commit** — `git add scripts/m3_mempalace_baseline.py crates/alice-daemon/tests/data/mempalace_baseline.json crates/alice-daemon/tests/import_quality.rs docs/benchmarks/m3.md && git commit -m "test: gold-set quality comparison against recorded MemPalace results"`

---

### Task 14: Acceptance on a copy of the owner's palace, docs, spec as built, handoff

**Why:** M3 is accepted when "the owner's palace imports with zero errors; gold-set search quality beats MemPalace; existing `mempalace_*` prompts work against Alice" (spec §12). The first and third are checked here on real data **copied**, with a throwaway home and no cloud; the second on the synthetic set (Task 13) plus a local-only set on real data if the owner supplies one.

**Files:**
- Modify: `docs/benchmarks/m3.md`, `README.md` and `README.th.md` (a short "Importing from MemPalace" and "MemPalace compatibility" section, same commit), `docs/superpowers/specs/2026-10-13-alice-core-design.md` (§8.2 and §9 as built; the Plan-decision deviations), `docs/superpowers/handoff/2026-10-14-m3-handoff.md` (create)
- No new test files; this task runs commands and records evidence.

- [ ] **Step 1: Refresh the copy** (the only step that touches the real palace; read-only backup API, never `cp` of a live WAL file; if `sqlite_exact.sqlite3-wal` is being written the backup API is still consistent):

```bash
export S=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice-m3/9facbaac-88e0-459a-9167-5ca357c0ce3e/scratchpad; rm -rf "$S/pal"; mkdir -p "$S/pal"
python3 -I - <<'E'
import sqlite3, os
S = os.environ["S"]
for name in ("sqlite_exact.sqlite3", "knowledge_graph.sqlite3"):
    src = sqlite3.connect(f"file:/Users/icue/Code/AI/memory_vault/{name}?mode=ro", uri=True, timeout=30)
    dst = sqlite3.connect(f"{S}/pal/{name}")
    src.backup(dst, pages=2000, sleep=0.01); dst.close(); src.close()
E
ls -la "$S/pal"   # also copy tunnels.json here if the palace has one: ls /Users/icue/Code/AI/memory_vault/tunnels.json 2>/dev/null
```
  Record the copy time. From here on, **only `$S/pal` is ever named in a command**.
- [ ] **Step 2: Dry run, then the real import** (throwaway home, fake embedder so no CPU goes to 119k embeddings yet):

```bash
export ALICE_HOME=$S/m3-home ALICE_EMBEDDER=fake; rm -rf "$ALICE_HOME"; mkdir -p "$ALICE_HOME"
source "$HOME/.cargo/env"; nice -n 15 cargo build --release -p alice-memory -j 4
A=target/release/alice
$A import mempalace --dry-run --palace "$S/pal" | tee "$S/dry-run.txt"
$A import mempalace --palace "$S/pal" --json > "$S/import-report.json"; echo "exit $?"
$A import mempalace --palace "$S/pal" | tee "$S/rerun.txt"            # must import 0
```
  Expected: exit 0; `errors` empty in the JSON; **accounting law** (also asserted by the importer, see Plan decisions): `drawers_seen == knowledge + raw_session + raw_file + duplicates + skipped + blocked_by_redaction`; the re-run reports every tier 0 and `already_imported == imported`. Inspect (counts only, never print drawer text): `jq '{tiers, duplicates, skipped, split, review_items, facts, tunnels, blocked_by_redaction}' "$S/import-report.json"` and the wing map's `inferred` / `global` shares. If more than 5% of knowledge drawers land in `global` or a wing/transcript folder maps to a surprising project, **stop and show the controller the map** (the owner may want `--map-wing` overrides; that is a decision, not a bug).
- [ ] **Step 3: Verify the data** (daemon stopped first: `$A daemon stop`; then read-only SQL on the throwaway db — it is ours): row counts per tier and project vs the report; `SELECT count(*) FROM memories WHERE judge_state <> 'skipped'` = 0; `PRAGMA quick_check`; no secret-shaped strings: `sqlite3 "$ALICE_HOME/alice.db" "SELECT count(*) FROM chunks WHERE text LIKE '%sk-ant-%' OR text LIKE '%ghp_%' OR text LIKE '%-----BEGIN%PRIVATE%'"` → 0. Compat on real shapes: `$A config get compat.mempalace` → true (auto-enabled); then through the HTTP route or MCP, run `mempalace_status`, `mempalace_list_wings`, `mempalace_search {"query": "<a non-sensitive phrase>", "wing": "pg_ai"}`, `mempalace_get_drawer` on a drawer id taken from the report's sample ids (the report carries none; take one id from the **copy** with the SQL below) and print only keys and counts: `sqlite3 "$S/pal/sqlite_exact.sqlite3" "select id from documents where collection_id=1 and id like 'diary_%' limit 1"`.
- [ ] **Step 4: Performance on real volume** (fake embedder, lexical + chunk path): with the imported home still in place, `alice daemon start`, run 200 `recall` calls (`--raw`) of varied words from a list of 40 common words and 40 rare ones, record p50/p95; record daemon RSS (`ps -o rss= -p <pid>`) at idle and after the recalls; record import wall time and records/second from the report. Put all four numbers and the machine in `docs/benchmarks/m3.md`. `alice daemon stop`.
- [ ] **Step 5: Real-model quality on the owner's data — ASK FIRST.** Embedding 119k chunks with the local e5 model is hours of CPU and the owner's data would be compared against MemPalace's answers. This is local (no cloud), but the brief reserves real-model evaluations to the controller. **Stop and ask in this pane:** (a) may the real model embed the copy now (`ALICE_EMBEDDER` unset, throwaway home), (b) which gold queries to use (the owner's own list, or queries auto-derived from the titles of held-out knowledge memories — a weaker proxy, labelled as such). Without both answers this step is reported as "not run", and the milestone report says so plainly.
- [ ] **Step 6: Docs.** `docs/benchmarks/m3.md` (import time, rate, RSS, recall latency, load test, gold-set table, the real-data counts); README "Importing from MemPalace" (the command, `--dry-run`, safe to re-run, what maps where in five lines, `compat.mempalace`) in both languages; spec §9 gets an "As built" note listing each deviation from the Plan decisions that touches it (decisions 2, 3, 7, 8, 15 below) and §8.2 notes that unsupported tools are answered but not listed; `docs/superpowers/handoff/2026-10-14-m3-handoff.md`: where things stand, ledger path, commands to resume, every ruling made, open items (the controller questions of step 5, the M2 coordination list below).
- [ ] **Step 7: Final checks and push.** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `env -u TYPESAFE_API_KEY nice -n 15 cargo test --workspace -j 4` (one heavy run), `pgrep -fl "alice daemon run"` empty, `git status` clean except intended files, `git merge --no-edit feat/m2-judge-settings` if it moved and re-run. Commit and `git push -u origin feat/m3-import` (never any other branch).

```bash
git add docs/benchmarks/m3.md README.md README.th.md docs/superpowers/specs/2026-10-13-alice-core-design.md docs/superpowers/handoff/2026-10-14-m3-handoff.md
git commit -m "docs: M3 results, README and handoff"
```

**M2 coordination list (write into the ledger; tell the controller at the end of Phase 2):** (1) migration number for `legacy_ids` (renumber on merge if M2 added one); (2) `rusqlite` `backup` feature line shared with M2 Task 14; (3) `judge_state = 'skipped'` is what the M2 pipeline (Task 12) must treat as "never judge"; (4) compat MCP gating reuses M2 Task 11's admin-tool gate; (5) `store::projects::upsert_project` gained placeholder adoption (new behaviour only for `import:` keys); (6) the knowledge graph of the owner's palace lives in the palace folder — `~/.mempalace/knowledge_graph.sqlite3` on this machine is empty.

---

## Plan decisions

Each entry: what the plan chose where the spec is silent or the real data forced a choice, and the cost if wrong. The spec wins everywhere else.

1. **The daemon runs the import; the CLI only starts it and polls.** Parsing, redaction and batching are worker-thread work (spec §2.2 rules 1–3), the daemon is the only process that opens `alice.db`, and a job survives a closed terminal. *Cost if wrong:* `alice import mempalace` starts the daemon if it is not running (as every other command does) and needs it reachable.
2. **Knowledge widening.** Spec §9: knowledge = diary drawers + drawers written by agents (`added_by` set, no source file). The owner's palace has 34,600 file-mined code drawers whose `added_by` is an agent name, so `added_by` alone would make them knowledge. The plan keeps the spec rule and adds one case: a drawer **with** a `source_file` in a curated room (`decisions`, `knowledge`, `status`, `progress`, `runbook`, `methods`, `bug-classes`, `issues`) written by an agent is knowledge too (31 drawers). *Cost if wrong:* up to ~31 notes sit in the raw tier (still searchable with `include_raw`/fallback).
3. **A `legacy_ids` table, not an index on `chunks.legacy_id`.** One lookup structure for compat resolution (including drawers collapsed into a survivor), the "already imported" check, and KG triples. *Cost if wrong:* ~120k extra small rows and a migration whose number must be reconciled with M2's.
4. **Imported projects are placeholders (`import:<slug>`) that the first real project of the same slug adopts.** Otherwise the first `remember` from the real checkout creates `pg-ai-1a2b3c` beside the imported `pg-ai`. *Cost if wrong:* two unrelated repositories that both slugify to `api` share the imported project (the imported wing was ambiguous anyway).
5. **Imported memories are `judge_state = 'skipped'`; imports never call the Judge.** The owner's history must not reach a cloud provider by accident (brief), and 5k judge calls are real money. *Cost if wrong:* imported knowledge is not related/superseded automatically; a later "judge backlog" command can do it deliberately.
6. **MemPalace's naive timestamps are read in the machine's UTC offset at import time** (MemPalace writes local naive times); the offset is printed in the report. *Cost if wrong:* times shift by the difference if the palace was built in another zone.
7. **The per-batch checkpoint is the `legacy_ids` rows written in the batch's own transaction**; the `sync_state` row is informational. No resume-from-cursor shortcut: drawer ids are hashes, so a drawer added since the last run can sort anywhere, and a cursor would skip it. *Cost if wrong:* a re-run re-plans every drawer (reads 1 GB again, seconds) before skipping what is stored.
8. **Collapse rules.** Raw: same source and identical text → one chunk (earliest `(chunk_index, filed_at, id)` kept). Knowledge: Alice's own `content_hash` (project, kind, title, body) at insert. Identical raw text in *different* files is kept (two files may legitimately hold the same lines). *Cost if wrong:* some repeated boilerplate chunks remain in the raw tier, which ranks at 0.6.
9. **Wing → project is data-driven** (a `wing_x` is an alias when a plain wing `x` exists, otherwise an agent wing whose project is inferred from the text) with a `--map-wing wing=project` override for names the data cannot decide (`wing_project_alice` → `alice`). *Cost if wrong:* a few agent-style wings land in `global` until the owner adds an override and re-imports (re-import only fills what moved; existing rows are not re-homed — see the handoff).
10. **Compat writes to an unknown wing go to the caller's current project** (with a `note`), never create a project (spec §3.2: agents never create containers). *Cost if wrong:* an old prompt that invents wings files into the current project instead of a new one.
11. **Compat `similarity` is the hit's score relative to the best hit** (1.0 for the best); Alice's fused scores are not cosines and must not be presented as such. *Cost if wrong:* a prompt that thresholds on MemPalace distances behaves differently.
12. **Unsupported `mempalace_*` tools are answered (not errors) but not listed.** Spec §8.2 says they are "not provided; calling them returns a message". *Cost if wrong:* none beyond a different answer shape.
13. **Compat gating is evaluated on every call** (no restart); the `tools/list_changed` notification is a nice-to-have. *Cost if wrong:* a client that cached the list sees the change at its next list.
14. **The gold comparison freezes the corpus before either system runs, records MemPalace's answers once, and the real-model run needs the controller's go.** *Cost if wrong:* a stale baseline (guarded by `the_baseline_file_matches_the_corpus`).
15. **The knowledge graph is read from the palace folder first, then `~/.mempalace/`.** On this machine the folder's file has the 159 triples and `~/.mempalace/knowledge_graph.sqlite3` is empty; the lane brief names the latter. *Cost if wrong:* an owner with only the home file is served by the fallback.
16. **The snapshot lives under `ALICE_HOME/import-tmp/<job>/`** (private, same volume, removed afterwards) and needs about the palace's size in free space (~1 GB for the owner's); a clear message says so when the volume is full. *Cost if wrong:* a machine short on disk must free space or import from a smaller copy.
17. **Embeddings are recomputed in the background; MemPalace's MiniLM vectors are ignored.** Spec §9. *Cost if wrong:* none.
18. **Only explicit tunnels that name two importable drawers become links.** Passive "tunnels" (shared room names) are not links. *Cost if wrong:* some cross-project relatedness is not carried over.
19. **Accounting law.** The report carries `drawers_seen` and `drawers_accounted` (imported memories + imported chunks + collapsed duplicates + skipped + blocked + already imported); a mismatch is an `errors` entry and a non-zero exit. This turns "zero errors" into a checkable statement. *Cost if wrong:* a legitimate new category not added to the sum shows up as a false error (fixed by naming it).

## Self-review notes (for the executor)

- **Spec §9 coverage:** read directly + `sqlite_exact` and `chroma` + KG (Tasks 2, 3, 6); snapshot with the backup API (2); mapping: transcript → raw session sources/chunks, file chunks → raw file, diary and agent drawers → knowledge with kinds from rooms, wings → projects through normalisation and aliases and agent wings inferred then `global`, rooms → topics, closets dropped, exact duplicates collapsed with `legacy_id` kept, KG with validity and predicate review items, tunnels → `relates_to` links (4, 5, 6, 7); AAAK text kept verbatim (5: text is never rewritten beyond NUL/NFC/redaction); embeddings in the background (8, decision 17); resumable and incremental (`--since`, `legacy_ids`) (1, 7, 8); final report (8). **§8.2:** the 15 mapped tools (10), unsupported tools answered with the Alice equivalent (10), opt-in setting enabled after import and live gating (8, 11). **§12 M3 acceptance:** zero errors on the owner's palace (14 steps 2–3), gold-set quality (13, 14 step 5), `mempalace_*` prompts work (10, 11, 14 step 3). **§10:** unit tests per module (all), concurrency regression with an import (12), crash safety during import (12), gold sets (13), interface tests (11), security corpus on imported text (8).
- **Names used across tasks** (check on every task): `DrawerMeta`, `Drawer`, `PalaceReader::{info,page,get,wing_counts}`, `Snapshot`, `WingMap::{build,target,projects,aliases,decide,project_for_transcript,note_transcript_dir}`, `ProjectDecision`, `Via`, `Tier`, `Survey`, `Planner::plan`, `Planned::{Memories,Chunks,Skipped,Collapsed}`, `PlannedMemory`, `PlannedChunk`, `KgPlan`, `PlannedFact`, `Tunnel`, `ImportBatch`, `ImportMemory`, `ImportSource`, `ImportFact`, `ImportProject`, `BatchOutcome`, `legacy_put/get/existing`, `ImportRequest`, `ImportStatus`, `ImportState`, `ImportReport`, `CompatCall`, `CompatResult`, `SUPPORTED`.
- **Fixes folded in while reviewing the plan against itself:** `ImportSource` carries `SourceInput.source_type` (no separate flag); `BatchOutcome` also carries `deferred_collapsed: Vec<(String, String)>` and `warnings: Vec<String>` (alias conflicts); the report carries `drawers_seen` / `drawers_accounted` (decision 19) and Task 8's first test asserts `errors.is_empty()` and `drawers_seen == drawers_accounted`.
