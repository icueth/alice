# Alice M3 — MemPalace Importer and Compatibility Layer — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship Alice milestone M3: `alice import mempalace` reads a MemPalace palace (`sqlite_exact` or `chroma` backend plus `knowledge_graph.sqlite3` and tunnels) from a read-only snapshot and imports it completely, resumably and incrementally into Alice's two tiers, projects, topics and knowledge graph; an opt-in `mempalace_*` MCP compatibility layer keeps existing prompts working; the import report says exactly what happened.

**Architecture:** Pure mapping logic lives in a new `alice-core::import` module (palace readers, wing → project map, tier/kind classification, survey, KG and tunnel mapping, report); one new store module `store::import` plus one `WriteOp::ImportBatch` apply a bounded batch (memories, source chunks, facts, links, review items) in a single short writer transaction; `alice-daemon::importer` is the worker thread that snapshots, surveys, redacts, batches and checkpoints; `alice-daemon::compat` translates `mempalace_*` calls onto the `Service`; the MCP server lists and routes those tools only while `compat.mempalace` is true. Nothing in M2's behaviour changes.

**Tech Stack:** Rust 1.99 (edition 2024), rusqlite 0.40 (bundled, `backup` feature), serde_json, chrono, strsim (existing `judge::text::predicate_similarity`), rmcp 3.5.1, axum 0.8, clap 4.6. No new crates except enabling rusqlite's `backup` feature (M2 Task 14 enables the same feature; a merge conflict on that one `Cargo.toml` line resolves to the union).

**Spec:** `docs/superpowers/specs/2026-10-13-alice-core-design.md` — §9 (importer), §8.2 (compat layer), §12 row M3 (acceptance), and what they rely on: §3.2–3.3 (data model, `legacy_id`), §5.1 (write path), §5.3 (redaction), §5.4 (review inbox), §5.5 (events), §8.1/§8.3/§8.4 (tools, CLI, HTTP), §10 (concurrency and crash tests, search-quality gold sets). Where the spec and this plan disagree the spec wins unless the "Plan decisions" section at the end says otherwise.

**Base:** branch `feat/m3-import`, created from `feat/m2-judge-settings` at 403c14f (M2 Tasks 1–9 done, Task 10 in re-review). M2 Task 11 (review inbox, admin MCP tools) is merged into this branch (M2 head 1ac3e10 at the time of the amendments); M2 Tasks 12–18 are not. Names in this plan are those of that commit. **M2 keeps moving**: at every task boundary run `git merge --no-edit feat/m2-judge-settings` if it moved, resolve conflicts, re-run the tests. **Amendments (2026-10-14, after the controller's review `m3-plan-review.md`, A1–A22, and the rulings in `m3-go.md`) are applied in place; tasks were split and renumbered (old → new): 1→1, 2→2, 3→3, 4→4, 5→5+6, 6→7, 7→8, 8→9+10+11, 9→12, 10→13+14, 11→15, 12→16, 13→17+18, 14→19+20.** **First step of every task:** read every file the task modifies as it is now; when a signature named here has changed, keep the behaviour this plan asks for and adapt the call. M3 never changes M2 behaviour; if M3 needs an M2 change, write it in the ledger and tell the controller.

**Real data seen while planning (a copy of the owner's palace, structure only, no content):** `sqlite_exact` backend; collections `mempalace_drawers` (119,179 documents) and `mempalace_closets` (4,416, dropped); metadata JSON per document with keys `wing`, `room`, `hall`, `source_file`, `added_by`, `ingest_mode` (`convos` | `registry` | absent), `extract_mode`, `chunk_index`, `filed_at`, `authored_at`, `agent`/`date`/`topic`/`type` (diary entries), `parent_drawer_id`; 302 wings (`sessions` 76,169 transcript drawers from 130 `.jsonl` files under `~/.claude/projects/<encoded cwd>/`; 218 agent-style `wing_*` wings with 1,851 drawers); 5,006 diary drawers (room `diary`, no `source_file`, ids `diary_wing_<agent>_<time>_<hash>`); 34,600 file-mined drawers with `added_by` set to an agent name (code chunks, so `added_by` alone does NOT mean knowledge); document text ≤ 10,981 chars (median 800); knowledge graph: 266 entities (all type `unknown`), 159 triples (2 ended). Embeddings are MiniLM 384-d and are ignored.

## Global Constraints

- Toolchain: Rust stable 1.99, edition 2024, `resolver = "3"`; crates `alice-core`, `alice-embed`, `alice-daemon`, binary package `alice-memory` (`[[bin]] name = "alice"`), `license = "MIT"`.
- IDs are ULIDs, timestamps RFC 3339 UTC with milliseconds and `Z` (`alice_core::ids::now_rfc3339`), HLC from the writer's `Hlc`.
- Performance budgets (spec §4.6) never relax: `remember` ack p95 < 50 ms and `recall` fast p95 < 200 ms **while an import runs** (Task 16 proves it); daemon RSS < 800 MB at 1M chunks.
- Only the daemon opens `alice.db`; all writes go through the single writer thread as `WriteOp`s. Importing parses and maps on a worker thread and hands the writer **bounded batches**: ≤ 64 KB of text and ≤ 200 records (memories + chunks + facts + links) per `ImportBatch`, so a `remember` queued behind an import waits for one batch at most (spec §2.2 rule 3).
- Limits from M1 stand (content ≤ 65,536 bytes, title ≤ 200 chars, ≤ 20 topics, chunk ≤ 32,768 bytes, importance 1–5). An imported drawer over a limit is split or truncated by the importer and counted in the report; it is never silently dropped and never an error.
- Redaction (spec §5.3) runs in the daemon on **every imported string that reaches the writer** — memory content, title, topics, chunk text, source `uri`/`session_id`/`agent`, KG subject/predicate/object, review payloads, project and alias names — and **on the whole drawer text before it is split** (a secret straddling a split point must not escape both halves); byte limits are re-checked after redaction (a marker can be longer than the secret). The importer uses the always-on redactor (`Redactors::for_root_always_on(None)`) regardless of `privacy.redact`, and the report says when the setting is off. Text already redacted is left as it is; in `block` mode the drawer/fact is skipped and counted by detector name, the import goes on. No secret appears in `alice.db`, the report, the `meta` marker or a log.
- **No implicit discovery of the owner's palace in the daemon.** `ServiceOptions::default()` has `mempalace_home: None`; only `process_env()` fills it. `start_import` without `palace` and without a configured home answers "no palace given; pass --palace <folder>". Defaults (`MEMPALACE_PALACE_PATH`, `MEMPAL_PALACE_PATH`, `~/.mempalace/config.json`) are resolved in the CLI only. Test harnesses set `HOME` to a temp dir and remove `MEMPALACE_PALACE_PATH`/`MEMPAL_PALACE_PATH`.
- **One audited code path touches the owner's files:** `import::palace::snapshot()` (Task 2), reached from `alice import mempalace` and from the hidden `alice import snapshot --palace P --out D` (Task 12) used for the acceptance copy. Real-palace commands record `ls -la` and `shasum -a 256` of the palace folder's files before and after.
- **Snapshot hygiene:** the snapshot (a copy of the owner's history, secrets included) lives in `ALICE_HOME/import-tmp/<job>/`; a drop guard removes it, the daemon sweeps `import-tmp/` at start, and M2 backups exclude it.
- The import thread holds M2 Task 14's `ActivityGuard` while it runs (the maintenance scheduler stays off the writer) and is joined by `Service::shutdown` before the writer is told to stop.
- A Judge failure never fails a write; **the import itself calls no Judge and sends nothing off the machine.** Imported memories are stored `status = 'active'` with `judge_state = 'skipped'` (M3 sets both itself in `apply_batch`; M2's judge pipeline only picks `'pending'`/`'retry'`, so they are not judged *themselves*). Once imported they are ordinary memories: a later `remember` judged by a cloud provider may include imported neighbours, and an HTTP embedding provider would receive every chunk — both follow the normal cloud settings (`SettingsHandle::cloud_allowed`, `privacy.cloud_denied_projects`). Therefore `start_import` **refuses** while `embedding.provider` is not `local` unless the request carries `allow_remote_embedding` (CLI `--allow-remote-embedding`). Embedding runs in the background after the import (lexical search works at once).
- Error text for agents and CLI users: one or two sentences saying what happened and what to do next.
- Server binds 127.0.0.1 only; every new route sits behind the existing bearer-token, `Host` and `Origin` guard.
- MCP core tool names stay `recall`, `remember`, `context`, `get_memory`, `update_memory`, `query_facts`, `assert_fact`; compat tools are named exactly as MemPalace names them (`mempalace_*`) and exist only while `compat.mempalace` is true.
- **The owner's real palace is read-only.** It lives at `/Users/icue/Code/AI/memory_vault` (backend `sqlite_exact`; config `~/.mempalace/config.json`; KG `~/.mempalace/knowledge_graph.sqlite3`). Never write, lock, migrate or delete anything there or under `~/.mempalace`. Any run against real data uses a copy made with the SQLite backup API into the session scratch directory and a throwaway `ALICE_HOME`. **Tests use synthetic fixtures built in code, never the real palace** (code, not committed binaries).
- **No cloud with real data.** Real palace content is never sent to Jev or any external service. If an acceptance step seems to need it, stop and ask the owner (via the controller) first.
- Never read or print `/Users/icue/Code/AI/project-alice/.env` or `TYPESAFE_API_KEY` (exported in this shell). Run every cargo test with `env -u TYPESAFE_API_KEY`. Secret-shaped test strings are built with `concat!` split inside the provider prefix (e.g. `concat!("gh", "p_…")`, `concat!("sk-", "ant-…")`); GitHub push protection rejects them otherwise.
- Heavy commands: `source "$HOME/.cargo/env"` first; `nice -n 15 cargo build|test|clippy -j 4`; one heavy run at a time; never run the 1M benches or real-model evaluations (the controller's lane owns those).
- Scratch prelude for every manual or shell step (`S` is this session's scratchpad; a later session substitutes its own): `export S=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice-m3/9facbaac-88e0-459a-9167-5ca357c0ce3e/scratchpad ALICE_HOME=$S/m3-home ALICE_EMBEDDER=fake ALICE_JUDGE_PROVIDER=heuristic; unset TYPESAFE_API_KEY; mkdir -p "$ALICE_HOME"`. A copy made with the SQLite backup API on 2026-10-14 20:50 is `$S/pal/` (`sqlite_exact.sqlite3`, `knowledge_graph.sqlite3`); Task 19 replaces it with one made by the audited snapshot command. Never use the real `~/.alice` or the real palace path in a command, with one exception: Task 19's snapshot step names the real palace folder as the *source* of the audited `alice import snapshot` command. **Every real-data command runs with `env -u TYPESAFE_API_KEY ALICE_JUDGE_PROVIDER=heuristic`** (the auto-started daemon inherits the environment).
- Never edit `~/.claude*`, `~/.codex` or any agent config; no release, no publish, no tag. Push only `feat/m3-import`; never push to or merge into `main` or `feat/m2-judge-settings`.
- Process tests leave no stray daemons: after every test run `pgrep -fl "alice daemon run"` prints nothing the run started.
- The workspace builds and its tests pass after **every** task, ignored tests included (`cargo test --workspace` compiles `#[ignore]` tests): a task that changes an API updates every caller.
- Before every commit: the task's tests, `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`. Stage files by name (never `git add -A`). Commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Every implementer confirms each named test exists with `git show --stat` + `grep -c "fn <test_name>"` before reporting (earlier lanes had silent edit failures: rustfmt moved the anchor and the edit did nothing).
- A shell hook routes commands through `rtk`; when output looks filtered or truncated, prefix `rtk proxy`. Docs are English; `README.th.md` changes in the same commit as `README.md`.

## Review Focus

The spec says what the importer must do; it is silent on the inputs below, and silence is not permission to break.

1. **Secrets inside imported history.** Transcripts and code chunks hold tokens, `.env` lines and passwords. Expected: every imported text field is redacted before the writer sees it; `block` mode skips that drawer and counts it (detector only); the report, the checkpoint and the logs never contain the matched text. Includes a secret straddling the 32,000-byte split point, a secret in a KG object and one in a transcript path. Owned by Tasks 5, 7 and 10 (test with `concat!`-built secrets in a transcript chunk, a diary and a code chunk).
2. **The palace is live and the palace is precious.** MemPalace may be writing (WAL file, hub lease), the path may be a symlink, missing, not a palace, a `chroma` directory with no `chroma.sqlite3`, or damaged. Expected: the source is opened read-only, a snapshot is taken with the backup API into a private temp dir, the source's database file is byte-identical afterwards (a WAL palace may gain empty `-shm`/`-wal` files), and a bad path gives one clear sentence — never a panic, never a partial import. MemPalace's `sqlite_exact` runs in WAL mode: a read-only open may leave empty `-shm`/`-wal` files and a live writer can restart a backup, so the snapshot has a restart limit and a time-box. Owned by Tasks 2, 3, 9 and 19.
3. **Interrupted and repeated imports.** `kill -9` mid-batch, running the import twice, running it while the palace gained drawers, `--since` with a bad date. Expected: every committed batch stands, a re-run finishes the rest, a run with nothing new imports zero records and creates zero duplicates, new drawers are picked up, a bad date is refused before any work. Owned by Tasks 8, 10, 11 and 16.
4. **Odd drawers.** Empty or whitespace-only text, text over 64 KB / 32 KB, NUL bytes, decomposed Thai sara am, missing `wing`/`room`/`chunk_index`, duplicate or negative `chunk_index`, the same text filed under `pg-ai`, `pg_ai` and `wing_pg_ai`, timestamps with and without zone, a wing that slugifies to nothing. Expected: each is imported sensibly or skipped with a counted reason; none is an error. Owned by Tasks 4, 5 and 6.
5. **Old prompts hitting the compat layer.** Unknown wing, a `drawer_id` of a raw chunk or of a collapsed duplicate, missing required arguments, numbers sent as strings, calls while `compat.mempalace` is off, tools Alice does not provide, `wing_pg_ai` as an alias of `pg-ai`. Expected: a helpful MemPalace-shaped answer (never a stack trace), the Alice equivalent named for unsupported tools, aliases resolved. Owned by Tasks 13, 14 and 15.

---

## File Structure

```
Cargo.toml                                   rusqlite + "backup" (M2 Task 14 adds the same; keep the union)
crates/alice-core/Cargo.toml                 feature `fixtures` (test-only palace builders, off by default)
crates/alice-core/src/
  lib.rs                                     + pub mod import
  import/mod.rs                              ImportRequest, ImportStatus, TextFilter, tz offset (Tasks 2, 5, 9)
  import/palace.rs                           Drawer, DrawerMeta, PalaceReader, snapshot (backup API, WAL-safe), sqlite_exact reader (Task 2)
  import/chroma.rs                           chroma reader (Task 3)
  import/wings.rs                            WingMap: wing/path → project, agent wings, aliases, inference (Task 4)
  import/plan.rs                             classify, kind_for_room, parse_time, split, TextFilter use (Task 5); Survey, Planner (Task 6)
  import/kg.rs                               KG + tunnels reader and mapping (Task 7)
  import/report.rs                           ImportReport, text rendering (Task 9)
  import/fixture.rs                          (cfg(any(test, feature = "fixtures"))) palace/KG/tunnel builders incl. a WAL palace (Tasks 2, 3, 7)
  store/sql/0004_import.sql                  legacy_ids table (renumber if M2 adds a migration after 0003) (Task 1)
  store/import.rs                            legacy_ids access, ImportBatch, apply_batch (Tasks 1, 8)
  store/compat.rs                            read-only queries for the compat layer (Tasks 13, 14)
  store/projects.rs                          adopt an `import:` placeholder project (Task 1)
  store/history.rs                           `import.batch` / `import.compat_enabled` are never undoable (Task 8)
crates/alice-daemon/src/
  importer.rs                                ImportManager: job thread, phases, batching (Tasks 9–11)
  compat.rs                                  mempalace_* → Service translation (Tasks 13, 14)
  compat_schema.rs                           the tools' names, descriptions and JSON input schemas (Task 15)
  writer.rs / service.rs / http.rs / mcp.rs / client.rs / config.rs   WriteOp::ImportBatch, Service import slot + compat, routes, MCP gate, shim switches
crates/alice/src/cmd.rs, main.rs             `alice import mempalace|status|snapshot` (Task 12)
crates/alice-daemon/tests/
  import_flow.rs                             end-to-end import of a fixture palace (Tasks 9–11)
  compat_flow.rs                             compat calls and MCP listing (Tasks 13–15)
  import_concurrency.rs                      20 clients during an import (Task 16)
  import_quality.rs, data/gold_corpus.json, data/mempalace_baseline.json   gold-set comparison (Tasks 17, 18)
scripts/gen_gold_corpus.py, scripts/m3_mempalace_baseline.py   synthetic corpus generator; local-only baseline recorder (Tasks 17, 18)
docs/benchmarks/m3.md                        import throughput, RSS, quality (Tasks 16, 18, 19)
docs/superpowers/handoff/…-m3-handoff.md     (Task 20)
```

## Task list

| # | Task | Depends on |
|---|---|---|
| 1 | Foundations: `legacy_ids` table and access, `import:` placeholder projects adopted by the real project | **M2 Task 12 merged** (migration numbering) |
| 2 | Palace snapshot (backup API, WAL-safe, bounded) and the `sqlite_exact` reader, with fixture builders | — (pure) |
| 3 | The `chroma` reader | 2 (pure) |
| 4 | Wings → projects: normalisation, agent wings, transcript paths, content inference, overrides | 2 (pure) |
| 5 | Classification, kind/topic/author/time mapping, redact-then-split sizing (`TextFilter`) | 2, 4 (pure) |
| 6 | The survey (collapse, stable ords) and the planner | 5 (pure) |
| 7 | Knowledge graph, predicates and tunnels (MemPalace's real tunnel shape; M2 review payload) | 2 (pure); finalise after M2 Task 13 |
| 8 | `WriteOp::ImportBatch`: one bounded, savepointed transaction for memories, chunks, facts, links, reviews and a marker | 1, 6, 7, M2 Tasks 12, 14 |
| 9 | Import job I: report types, request/status, preflight, lifecycle slot, snapshot → survey → dry-run counts, routes | 8 |
| 10 | Import job II: knowledge and raw phases, redaction, atomic batching, resume | 9 |
| 11 | Import job III: graph, tunnels, collapsed and mention phases, accounting law, auto-enable, failure and shutdown | 10, M2 Task 13 |
| 12 | `alice import mempalace|status|snapshot` CLI and flags | 11 |
| 13 | Compat core I: read tools (status, search, list_wings, list_rooms, get_drawer), argument handling, unsupported table | 11 |
| 14 | Compat core II: write tools (add/update/delete, diary, the five kg tools) | 13 |
| 15 | Compat on MCP: schemas, the shared switches gate, stdio shim forwarding | 14, M2 Task 11 (merged) |
| 16 | Concurrency and crash safety during an import | 11, M2 Task 16 |
| 17 | Gold corpus: generator and frozen synthetic corpus | — |
| 18 | Gold harness (file-driven), baseline recorder, Alice-vs-MemPalace run | 14, 17, M2 Tasks 13, 16 |
| 19 | Acceptance run on a snapshot of the owner's palace, real-query quality check | 12, 15, 18, M2 Task 18 |
| 20 | Docs, spec as built, handoff, push | 19 |

Landing order (controller ruling): Tasks 2–7 first (pure), then as M2 lands: M2 Task 12 → Task 1; after M2 Tasks 13/14 → Tasks 8–12; after M2 Task 16 → 13–16; after M2 Task 18 → 17–20 as written above. Merge `feat/m2-judge-settings` at every task boundary.

---

### Task 1: Foundations — `legacy_ids` and placeholder projects

**Prerequisite:** M2 Task 12 (which adds `0003_judge.sql`, `CURRENT_VERSION = 3`) is merged into this branch; do not start before that — the new file is `0004_import.sql`. If M2 added another migration after 0003, use the next free number.

**Note on adoption (decision 4):** only an exact slug adopts a placeholder: a real checkout named `project-alice` does not adopt the imported `alice`. A placeholder that has an alias equal to the real slug is also adopted (`project_aliases` lookup before the slug-conflict logic); one test pins it.

**Why:** `memories.legacy_id` is a single column, but spec §9 collapses duplicates ("every original id is stored in `legacy_id` for compat lookups") and the compat layer must resolve a collapsed drawer's old id to the survivor; raw chunks have `chunks.legacy_id` but no index (a 1M-row scan per lookup). One small table solves both and doubles as the "already imported" index for incremental runs. Imported projects exist before any checkout does; when an agent later works in the real checkout, the real project must **adopt** the placeholder instead of becoming `pg-ai-1a2b3c` (that would re-create the wing sprawl the product exists to remove).

**Files:**
- Create: `crates/alice-core/src/store/sql/0004_import.sql` (the next free number after M2's; check `ls crates/alice-core/src/store/sql` and `MIGRATIONS`)
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

- [ ] **Step 1: Write the failing tests** (in `store/import.rs` `mod tests`; use the existing helper `conn() -> (tempfile::TempDir, Connection)` of `store/kg.rs` tests (copy it into this module's tests — it opens a migrated writer connection); `let (_d, conn) = conn();` replaces every `fresh()` below)

```rust
#[test]
fn legacy_ids_round_trip_and_ignore_repeats() {
    let (_d, conn) = conn();
    legacy_put(&conn, "drawer_a", "memory", "M1").unwrap();
    legacy_put(&conn, "drawer_a", "memory", "OTHER").unwrap(); // first writer wins
    let hit = legacy_get(&conn, "drawer_a").unwrap().unwrap();
    assert_eq!((hit.target_type.as_str(), hit.target_id.as_str()), ("memory", "M1"));
    assert!(legacy_get(&conn, "nope").unwrap().is_none());
}

#[test]
fn existing_finds_only_stored_ids_even_for_thousands() {
    let (_d, conn) = conn();
    for i in 0..1200 { legacy_put(&conn, &format!("d{i}"), "chunk", &format!("C{i}")).unwrap(); }
    let ask: Vec<String> = (1100..1300).map(|i| format!("d{i}")).collect();
    let got = legacy_existing(&conn, &ask).unwrap();
    assert_eq!(got.len(), 100);
    assert!(got.contains("d1199") && !got.contains("d1200"));
}
```

and in `store/projects.rs` `mod tests`:

```rust
// uses the existing `conn()` and `pref(key, slug)` helpers of store/projects.rs tests
#[test]
fn a_real_project_adopts_the_import_placeholder_of_the_same_slug() {
    let (_d, conn) = conn();
    let placeholder = ProjectRef { key: "import:pg-ai".into(), slug: "pg-ai".into(), name: "pg-ai".into(), root: None, remote: None };
    let imported = upsert_project(&conn, &placeholder).unwrap();
    let real = ProjectRef { key: "github.com/icueth/pg-ai".into(), slug: "pg-ai".into(), name: "PG AI".into(), root: None, remote: Some("github.com/icueth/pg-ai".into()) };
    let adopted = upsert_project(&conn, &real).unwrap();
    assert_eq!(adopted.id, imported.id, "same project, no pg-ai-<hash> twin");
    assert_eq!(adopted.slug, "pg-ai");
    assert_eq!(adopted.name, "PG AI", "the real project's name replaces the placeholder's");
    assert_eq!(find_by_key(&conn, "github.com/icueth/pg-ai").unwrap().unwrap().id, imported.id);
    assert!(find_by_key(&conn, "import:pg-ai").unwrap().is_none());
}

#[test]
fn a_placeholder_is_not_adopted_by_another_placeholder_or_a_different_slug() {
    let (_d, conn) = conn();
    let a = upsert_project(&conn, &ProjectRef { key: "import:alpha".into(), slug: "alpha".into(), name: "alpha".into(), root: None, remote: None }).unwrap();
    let other = upsert_project(&conn, &ProjectRef { key: "path:/x/beta".into(), slug: "beta".into(), name: "beta".into(), root: None, remote: None }).unwrap();
    assert_ne!(a.id, other.id);
    // a second import: key identical → the same project
    let again = upsert_project(&conn, &ProjectRef { key: "import:alpha".into(), slug: "alpha".into(), name: "alpha".into(), root: None, remote: None }).unwrap();
    assert_eq!(again.id, a.id);
}
```

```rust
#[test]
fn a_placeholder_is_also_adopted_through_an_alias_equal_to_the_real_slug() {
    let (_d, conn) = conn();
    let imported = upsert_project(&conn, &pref("import:alice", "alice")).unwrap();
    add_alias(&conn, "project-alice", &imported.id).unwrap();           // what the importer's wing alias table does
    let adopted = upsert_project(&conn, &pref("github.com/icueth/alice", "project-alice")).unwrap();
    assert_eq!(adopted.id, imported.id, "no project-alice-<hash> twin");
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

`upsert_project`: after the `find_by_key` early return and before the slug-conflict logic, when `!p.key.starts_with(PLACEHOLDER_KEY_PREFIX)` look for `SELECT id, slug, name FROM projects WHERE slug = ?1 AND key LIKE 'import:%'` with `slugify(&p.slug)`; if found, `UPDATE projects SET key = ?1, name = ?2, git_remote = ?3, root_path = COALESCE(?4, root_path) WHERE id = ?5` with `?4` only when the root exists on disk (the same `root.exists()` guard as the refresh branch above — never store a dead root) and return it. Placeholders never adopt placeholders (the early `find_by_key` already returns the same project for the same key).

- [ ] **Step 4: Run the tests** — same command. Expected: PASS. Also `nice -n 15 cargo test -p alice-core -j 4` (update the real migration tests, not an assumed count: Task 4's `a_v1_database_migrates_to_v2…` and M2 Task 12's v3 test hard-code the list; `CURRENT_VERSION` becomes 4 and the const assert needs the new last entry; the daemon tests that open fresh databases must still pass: `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --lib -j 4`).
- [ ] **Step 5: Confirm the named tests landed** (`git show --stat HEAD` after commit, `grep -c "fn legacy_ids_round_trip_and_ignore_repeats\|fn a_real_project_adopts" …` = 2 in the right files), then fmt, clippy, commit:

```bash
git add crates/alice-core/src/store/sql crates/alice-core/src/store/import.rs crates/alice-core/src/store/mod.rs crates/alice-core/src/store/migrations.rs crates/alice-core/src/store/projects.rs
git commit -m "feat: legacy_ids table and adoption of import placeholder projects"
```

---

### Task 2: Palace snapshot and the `sqlite_exact` reader

**Why:** this is the only code that opens the owner's files. It must work on a **live WAL database** (MemPalace's `sqlite_exact` runs `PRAGMA journal_mode=WAL`), must not livelock against a writer, and must be small enough to audit in one sitting.

**Files:**
- Create: `crates/alice-core/src/import/mod.rs`, `crates/alice-core/src/import/palace.rs`, `crates/alice-core/src/import/fixture.rs`
- Modify: `crates/alice-core/src/lib.rs` (`pub mod import;`), `crates/alice-core/Cargo.toml` (`[features] fixtures = []`), root `Cargo.toml` (rusqlite features `["bundled", "backup"]`; after merging M2 Task 14 keep one `backup`), `crates/alice-daemon/Cargo.toml` and `crates/alice/Cargo.toml` (`[dev-dependencies] alice-core = { path = "../alice-core", features = ["fixtures"] }`)
- Test: `crates/alice-core/src/import/palace.rs` (`mod tests`). `import::fixture` is compiled under `#[cfg(any(test, feature = "fixtures"))]`, so `cargo test -p alice-core import::` needs no feature flag.

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
    /// True when the metadata JSON could not be parsed at all (the drawer is then skipped, never guessed at).
    pub unreadable: bool,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Drawer { pub id: String, pub text: String, pub meta: DrawerMeta }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend { SqliteExact, Chroma }
#[derive(Debug, Clone)]
pub struct PalaceInfo { pub backend: Backend, pub path: std::path::PathBuf, pub drawers: u64 }
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub dir: std::path::PathBuf, pub backend: Backend, pub source: std::path::PathBuf,
    /// Copies of the knowledge graph and the tunnels file made into `dir`, when the palace has them.
    pub kg: Option<std::path::PathBuf>, pub tunnels: Option<std::path::PathBuf>,
    pub notes: Vec<String>,   // e.g. "palace is in WAL mode; snapshot read through SQLite's WAL"
}
pub trait PalaceReader: Send {
    fn info(&self) -> &PalaceInfo;
    /// Drawers with id greater than `after` (byte order), at most `limit`, optionally only those filed at or after `since`.
    fn page(&self, after: Option<&str>, limit: usize, since: Option<&str>) -> Result<Vec<Drawer>>;
    /// Drawers by id (missing ids are skipped).
    fn get(&self, ids: &[String]) -> Result<Vec<Drawer>>;
    /// (wing, drawer count) over the whole palace.
    fn wing_counts(&self) -> Result<Vec<(String, u64)>>;
}
pub const DEFAULT_COLLECTION: &str = "mempalace_drawers";
pub fn detect_backend(palace: &Path) -> Result<Backend>;
/// `mempalace_home` (usually `~/.mempalace`, `None` in tests) is only used to find a legacy KG / tunnels file.
pub fn snapshot(palace: &Path, dest_dir: &Path, mempalace_home: Option<&Path>) -> Result<Snapshot>;
pub fn open_snapshot(s: &Snapshot, collection: Option<&str>) -> Result<Box<dyn PalaceReader>>;
```
  `import::fixture`: `pub struct DrawerSpec { pub id: String, pub text: String, pub meta: DrawerMeta }`; `pub fn build_sqlite_exact(dir: &Path, drawers: &[DrawerSpec]) -> PathBuf` (journal_mode DELETE) and `pub fn build_sqlite_exact_wal(dir: &Path, drawers: &[DrawerSpec]) -> WalPalace` where `WalPalace` keeps a **second connection with an open write transaction** (so `-wal` and `-shm` exist and a writer is active) until dropped. Both create `dir/sqlite_exact.sqlite3` with exactly MemPalace's tables (`meta`, `collections`, `documents`) and collections `mempalace_drawers` + `mempalace_closets` (one closet row).

**Snapshot rules (A8, A9):**
1. The source database is opened `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX` (never `create`); the destination is a fresh file in `dest_dir` (directory mode 0700 on Unix).
2. Probe the header (bytes 18–19 = `2`,`2` means WAL) and note it. A WAL database is read through SQLite's own WAL handling (the backup API takes a read snapshot; writers are not blocked).
3. `Backup::step(500)` loop with a restart detector: read `progress()`; if `pagecount` grows or `remaining` jumps up between steps (a writer touched the database), count a restart. After **3 restarts** switch to one `step(-1)` (a single read transaction; in WAL mode this is consistent and does not block the writer). If the database is **not** WAL and a 4th restart happens, fail with "MemPalace is writing to <file>; stop it (or retry in a minute) and run the import again". Whole snapshot time-boxed at 10 minutes ("the snapshot did not finish in 10 minutes; …").
4. After the main file, copy `knowledge_graph.sqlite3` (palace folder first, then `mempalace_home`, same method and limits) and `tunnels.json` (candidates in order: `dirname(palace)/tunnels.json` — MemPalace's own location — then `<palace>/tunnels.json`, then `mempalace_home/tunnels.json`; plain file copy of a small JSON, size-capped at 16 MB). Nothing else is read.
5. The only entries the snapshot may add next to the source are empty `-shm`/`-wal` files that SQLite itself creates for a read-only WAL open; the source's main file is never modified (tests assert its SHA-256).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn snapshot_leaves_a_delete_mode_palace_byte_identical_and_reads_every_drawer() {
    let src = tempfile::tempdir().unwrap();
    build_sqlite_exact(src.path(), &sample(250));
    let before = hash_dir(src.path());
    let dst = tempfile::tempdir().unwrap();
    let snap = snapshot(src.path(), dst.path(), None).unwrap();
    assert_eq!(hash_dir(src.path()), before, "the palace must not change");
    assert_eq!(snap.backend, Backend::SqliteExact);
    let r = open_snapshot(&snap, None).unwrap();
    assert_eq!(r.info().drawers, 250); // closets are not counted
    let mut seen = Vec::new(); let mut after: Option<String> = None;
    loop { let p = r.page(after.as_deref(), 100, None).unwrap(); if p.is_empty() { break; }
        after = Some(p.last().unwrap().id.clone()); seen.extend(p.into_iter().map(|d| d.id)); }
    assert_eq!(seen.len(), 250);
    let mut sorted = seen.clone(); sorted.sort(); assert_eq!(seen, sorted, "pages are in id order");
}

#[test]
fn a_live_wal_palace_with_an_open_writer_snapshots_without_changing_its_main_file() {
    let src = tempfile::tempdir().unwrap();
    let wal = build_sqlite_exact_wal(src.path(), &sample(120));          // a second connection holds an open write
    let main = src.path().join("sqlite_exact.sqlite3");
    let before = sha256_file(&main);
    let dst = tempfile::tempdir().unwrap();
    let snap = snapshot(src.path(), dst.path(), None).unwrap();
    assert_eq!(sha256_file(&main), before, "main database file untouched");
    let new: Vec<_> = list_names(src.path()).into_iter().filter(|n| n != "sqlite_exact.sqlite3").collect();
    assert!(new.iter().all(|n| n.ends_with("-shm") || n.ends_with("-wal")), "only SQLite's own WAL files may exist: {new:?}");
    assert_eq!(open_snapshot(&snap, None).unwrap().info().drawers, 120, "committed rows only; the writer's open transaction is not in the copy");
    assert!(snap.notes.iter().any(|n| n.contains("WAL")));
    drop(wal);
}

#[test]
fn a_writer_that_keeps_committing_cannot_stall_the_snapshot() {
    /* build_sqlite_exact_wal with a background thread committing a tiny insert every 2 ms into a scratch table of the same file (not the drawers collection);
       snapshot() must return Ok within 60 s (test timeout) and report a note "restarted N times" with N ≥ 0; the copy passes PRAGMA quick_check */
}

#[test]
fn a_delete_mode_palace_that_is_being_written_fails_with_a_clear_sentence() {
    /* journal_mode=DELETE db plus a thread that grabs the write lock repeatedly via BEGIN IMMEDIATE; snapshot() → Err containing "MemPalace is writing" or succeeds (race) —
       assert: never panics, never leaves a partial file in dest_dir on Err */
}

#[test]
fn since_keeps_only_drawers_filed_at_or_after_the_date() {
    /* 3 drawers filed 2026-01-01 / 2026-06-01 / no filed_at → since 2026-03-01 returns the second and the one with no filed_at (cannot be shown older);
       comment in the code: `since` compares ISO text, so a bare date works against naive and Z strings; a zoned --since is compared as text (decision 20) */
}

#[test]
fn metadata_is_parsed_with_missing_keys_and_wrong_types() {
    /* chunk_index stored as "7" (string) and 7.0 and 7 → Some(7), -3 stays Some(-3) (Task 6 treats negatives as unusable); wing missing → ""; hall null → None;
       metadata_json = "not json" → meta.unreadable == true with the id kept; NUL bytes in document are kept as they are */
}

#[test]
fn bad_paths_say_what_to_do() {
    let d = tempfile::tempdir().unwrap();
    let e = snapshot(&d.path().join("missing"), d.path(), None).unwrap_err().to_string();
    assert!(e.contains("does not exist") && e.contains("--palace"), "{e}");
    std::fs::write(d.path().join("sqlite_exact.sqlite3"), b"not sqlite").unwrap();
    let dst = tempfile::tempdir().unwrap();
    let e = snapshot(d.path(), dst.path(), None).unwrap_err().to_string();
    assert!(e.contains("sqlite_exact.sqlite3") && e.contains("damaged"), "{e}");
    assert!(std::fs::read_dir(dst.path()).unwrap().next().is_none(), "nothing left behind on failure");
    let empty = tempfile::tempdir().unwrap();
    assert!(snapshot(empty.path(), dst.path(), None).unwrap_err().to_string().contains("not a MemPalace palace"));
}

#[test]
fn a_symlinked_palace_directory_is_followed_and_still_not_modified() { /* unix only: symlink dir → fixture; snapshot ok; hash unchanged */ }

#[test]
fn kg_and_tunnels_are_copied_from_mempalaces_own_locations() {
    /* palace dir P/palace with fixture + P/palace/knowledge_graph.sqlite3 + P/tunnels.json (the PARENT folder, MemPalace's location) →
       snap.kg and snap.tunnels are Some and live inside dst; with only mempalace_home/knowledge_graph.sqlite3 → used as fallback; neither → None */
}
```

- [ ] **Step 2: Run to verify they fail** — `source "$HOME/.cargo/env"; env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-core import:: -j 4`. Expected: FAIL (module missing).
- [ ] **Step 3: Implement.**
  - `detect_backend`: `sqlite_exact.sqlite3` present → `SqliteExact`; else `chroma.sqlite3` → `Chroma`; else `Err(invalid("<path> is not a MemPalace palace (no sqlite_exact.sqlite3 or chroma.sqlite3); pass the folder that holds it with --palace"))`. A missing directory: "`<path>` does not exist; pass --palace <folder> or run `mempalace status` to see where your palace is".
  - Open/`not a database` errors: "`<file>` cannot be read as a SQLite database ({e}); the palace may be damaged. Nothing was copied."
  - Reader: collection id by name (`collection` argument, default `DEFAULT_COLLECTION`); `page` = `SELECT id, document, metadata_json FROM documents WHERE collection_id = ?1 AND id > ?2 [AND (json_extract(metadata_json,'$.filed_at') IS NULL OR json_extract(metadata_json,'$.filed_at') >= ?3)] ORDER BY id LIMIT ?4` (never reads the `embedding` BLOB); `get` uses a chunked `IN (...)`; `wing_counts` = `SELECT json_extract(metadata_json,'$.wing'), count(*) … GROUP BY 1` (NULL → `""`). Meta parsing is tolerant: `chunk_index` accepts int, float or numeric string; other scalars stringified; unparsable JSON → default meta with `unreadable = true`.
  - On any `Err` the destination files written so far are removed.
- [ ] **Step 4: Run the tests** — Expected: PASS; also `nice -n 15 cargo build -p alice-core --features fixtures -j 4`.
- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/alice-core/Cargo.toml crates/alice-core/src/lib.rs crates/alice-core/src/import crates/alice-daemon/Cargo.toml crates/alice/Cargo.toml
git commit -m "feat: palace snapshot and sqlite_exact reader"
```

---

### Task 3: The `chroma` reader

**Files:**
- Create: `crates/alice-core/src/import/chroma.rs`
- Modify: `crates/alice-core/src/import/palace.rs` (`snapshot`/`open_snapshot` dispatch on `Backend::Chroma`; `chroma.sqlite3` goes through the same WAL-safe, restart-limited snapshot routine), `crates/alice-core/src/import/fixture.rs` (`build_chroma`)
- Test: `crates/alice-core/src/import/chroma.rs` (`mod tests`)

**Interfaces:**
- Consumes: `PalaceReader`, `Drawer`, `DrawerMeta`, `snapshot` (Task 2).
- Produces: `pub struct ChromaReader` implementing `PalaceReader`; `fixture::build_chroma(dir, &[DrawerSpec]) -> PathBuf` (creates `dir/chroma.sqlite3`; same `cfg(any(test, feature = "fixtures"))` as the other fixtures). `open_snapshot(&snap, Some(collection))` selects the collection.

Facts verified against chromadb 1.5.7 while planning: only `chroma.sqlite3` is needed (documents and metadata live in SQLite; the HNSW folders are vectors and are ignored). Tables: `collections(id TEXT PK, name, dimension, database_id, …)`; `segments(id, type, scope, collection)` where the metadata segment has `scope = 'METADATA'`; `embeddings(id INTEGER PK, segment_id, embedding_id TEXT, seq_id BLOB, created_at, UNIQUE(segment_id, embedding_id))`; `embedding_metadata(id INTEGER REFERENCES embeddings(id), key TEXT, string_value TEXT, int_value INTEGER, float_value REAL, bool_value INTEGER, PRIMARY KEY(id, key))`; the document text is the metadata key `chroma:document` (string_value). A drawer's id is `embeddings.embedding_id`; its metadata is the other keys of its `embedding_metadata` rows.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn chroma_drawers_come_out_like_sqlite_exact_ones() {
    let src = tempfile::tempdir().unwrap();
    let specs = sample_specs(120);
    build_chroma(src.path(), &specs);
    let dst = tempfile::tempdir().unwrap();
    let snap = snapshot(src.path(), dst.path(), None).unwrap();
    assert_eq!(snap.backend, Backend::Chroma);
    let r = open_snapshot(&snap, None).unwrap();
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
- [ ] **Step 3: Implement.** `build_chroma` creates the five tables above with the exact DDL printed in "Facts verified" (plus a `databases(id)` row so the FK is satisfied), one collection `mempalace_drawers`, a `VECTOR` and a `METADATA` segment, and one `embeddings` row + `embedding_metadata` rows per spec (`chroma:document`, then `wing`/`room`/… as `string_value`, `chunk_index` as `int_value`). `ChromaReader::page`: `SELECT e.id, e.embedding_id FROM embeddings e JOIN segments s ON s.id = e.segment_id WHERE s.scope='METADATA' AND s.collection = ?1 AND e.embedding_id > ?2 ORDER BY e.embedding_id LIMIT ?3`, then one query loading all `embedding_metadata` rows of those internal ids and folding them into `Drawer`/`DrawerMeta` with the same tolerant typing as Task 2 (shared helper `meta_from_pairs(impl Iterator<Item=(String, MetaValue)>)` lives in `palace.rs`; use it from both readers so they cannot drift). `since` filters in Rust after the fold for Chroma when the `filed_at` key is a string (ISO strings compare lexicographically); keep paging until `limit` drawers pass the filter or the cursor ends. `info().drawers` = count of embeddings of the metadata segment of the drawers collection. If the collection is missing: `Err(invalid("chroma palace has no collection \"mempalace_drawers\" (it has: {names}); set collection_name in the palace's config or pass --collection"))`. (The CLI flag `--collection` is added in Task 12; the reader takes the collection name as a parameter with default `DEFAULT_COLLECTION`.)
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
    /// Called by the survey (Task 6) for each transcript path: a folder that decodes to a project nobody named adds that project (wing spelling `transcript:<folder>`).
    pub fn note_transcript_dir(&mut self, source_file: &str);
}
```

**Rules (all pinned by tests below):**
1. `canon(w) = slugify(w)`. Empty → `Unnamed` (drawers go to `global`, via `Global`). `slugify("wing_") == "wing"`: a wing named exactly `wing_` is an ordinary project wing, not an unnamed one.
2. `sessions` (canon) → `Sessions`.
3. A wing whose canon starts with `wing-` is *prefixed*. Its stem is the canon without that prefix. If the stem equals the canon of any non-prefixed wing (other than `sessions`) the wing is an **alias** of that project (`Project`, via `Alias`); otherwise it is `Agent { agent: stem }`.
4. A non-prefixed wing is `Project { slug: canon }` (via `Wing`).
5. `overrides` (`--map-wing wing=project`) win: the key is matched against the wing exactly or by canon; the value is slugified; via `Override`. An override whose project is not otherwise known creates that project.
6. `decide(meta, text)`: `Project` → its slug. `Agent` → `infer(text)` (below) else `global` (via `Inferred` / `Global`), `agent = Some(stem)` either way. `Sessions` or any drawer with a transcript `source_file` → `project_for_transcript(source_file)` else the wing rule else content inference else `global`.
7. `infer(text)`: look at the first 4,000 chars lower-cased; for each known project slug of ≥ 4 characters count whole-token occurrences of its three spellings (`pg-ai`, `pg_ai`, `pgai`); the project with the strictly highest count wins; a tie or no hit → none. "Whole token" = the neighbours are not ASCII alphanumeric.
8. `project_for_transcript(path)`: take the folder name (parent of the file, e.g. `-Users-icue-Code-AI-devspace`), split on `-`, lower-case; the known project whose slug tokens are a **suffix** of those tokens wins, longest slug first; a slug shorter than 4 characters never matches by suffix (a project `app` must not swallow every folder ending in `-app`). If none matches, and the tokens contain `code`, the tokens after the first `code` (dropping a leading `ai` when something follows) joined with `-` become a new project slug (listed by `projects()` as coming from transcripts). Otherwise `None`.

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
- [ ] **Step 3: Implement** the rules above. Keep the wing → target table in a `HashMap<String, WingTarget>` keyed by the **raw** wing string (so `target()` is an exact lookup; unknown wings seen later by `decide` are classified on the fly with the same rules against the canon set built at `build`). `wing_` / `wing-` prefix test works on `canon` (`starts_with("wing-")`); a wing named exactly `wing_` is not prefixed (its canon is `wing`, no dash). `project_for_transcript` returns slugs as stored in `projects()`; a slug first seen through a transcript is added to an interior `RefCell`-free `Vec` built at `build` time by scanning `wings` only — to learn transcript-derived projects the planner calls `WingMap::note_transcript_dir(&mut self, source_file)` during the survey (Task 6), which inserts the derived slug into the project table with wing spelling `transcript:<folder>`.
- [ ] **Step 4: Run the tests** — PASS.
- [ ] **Step 5: Commit** — `git add crates/alice-core/src/import/wings.rs crates/alice-core/src/import/mod.rs && git commit -m "feat: map MemPalace wings and transcript folders to projects"`

---

### Task 5: Classification, mapping and sizing

**Why:** the mapping table of spec §9, made exact against the real palace shape, for **one drawer at a time**. Pure: no database, no writer. It also fixes the order that makes redaction safe: clean → **redact the whole text** → split → re-check sizes.

**Files:**
- Create: `crates/alice-core/src/import/plan.rs` (this task: `classify`, `kind_for_room`, `parse_time`, `split_utf8`, `TextFilter`, `Mapper`; Task 6 adds `Survey` and `Planner` to the same file)
- Modify: `crates/alice-core/src/import/mod.rs` (`pub mod plan;`, `pub fn local_offset() -> chrono::FixedOffset`, `TextFilter`)
- Test: `crates/alice-core/src/import/plan.rs` (`mod tests`)

**Interfaces:**
- Consumes: `Drawer`, `DrawerMeta` (Task 2); `WingMap`, `ProjectDecision`, `Via` (Task 4); `store::memories::NewMemoryInput`, `store::sources::{SourceInput, ChunkInput}`, `model::{Kind, SourceType, Author, limits}`, `normalize::{nfc, normalize_topic, sha256_hex}`.
- Produces:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tier { Knowledge, RawSession, RawFile, Skip(&'static str) }
pub fn classify(id: &str, meta: &DrawerMeta) -> Tier;
pub fn kind_for_room(room: &str) -> Kind;
pub fn parse_time(s: &str, assume: chrono::FixedOffset) -> Option<String>; // RFC 3339 ms Z
/// Splits `text` into parts of at most `max` bytes at the best of `prefer` separators (kept with the left part), else at a char boundary.
pub fn split_utf8(text: &str, max: usize, prefer: &[&str]) -> Vec<String>;

/// Redaction hook (the importer wraps `Redactor::check`; tests pass `NoFilter`). `Err` carries detector names: the drawer is blocked.
pub trait TextFilter: Sync {
    fn filter(&self, field: &str, text: &str) -> Result<String, Vec<String>>;
    /// True when something was replaced (for the report).
    fn changed(&self, before: &str, after: &str) -> bool { before != after }
}
pub struct NoFilter;

#[derive(Debug, Clone)]
pub struct PlannedMemory { pub legacy_id: String, pub project: ProjectDecision, pub input: NewMemoryInput }
#[derive(Debug, Clone)]
pub struct PlannedChunk { pub legacy_id: String, pub project: ProjectDecision, pub source: SourceInput, pub chunk: ChunkInput }
#[derive(Debug, Clone)]
pub enum Planned {
    /// All parts of one drawer; they are written together or not at all.
    Memories(Vec<PlannedMemory>),
    Chunks(Vec<PlannedChunk>),
    Skipped { reason: &'static str },
    Blocked { detectors: Vec<String> },
    Collapsed { keeper: String },
}
pub struct Mapper<'a> { pub wings: &'a WingMap, pub assume: chrono::FixedOffset, pub filter: &'a dyn TextFilter }
impl Mapper<'_> {
    /// `ord` is the chunk position inside its source (Task 6 supplies it); ignored for knowledge.
    pub fn map(&self, d: &Drawer, ord: i64, span: (Option<String>, Option<String>)) -> Planned;
}
```
(Task 6's `Planner` wraps `Mapper` with the survey's ords and collapse list.)

**Rules (pinned by tests):**
1. `classify` in this order: `meta.unreadable` → `Skip("unreadable metadata")`; `ingest_mode == registry`, `room == _registry` or id starts `_reg_` → `Skip("MemPalace registry entry")`; `ingest_mode == convos`, or `source_file` ends `.jsonl`, or wing canon `sessions` → `RawSession`; room `diary`, `entry_type == diary_entry` or id starts `diary_` → `Knowledge`; no `source_file` → `Knowledge`; room ∈ {`decisions`, `knowledge`, `status`, `progress`, `runbook`, `methods`, `bug-classes`, `issues`} **and** `added_by` set and ≠ `mempalace` → `Knowledge` (deliberate widening of spec §9, Plan decision 2); everything else `RawFile`. Text empty after trimming (NUL removed) → `Skip("empty text")` (decided by `map`, which sees the text).
2. `kind_for_room` (room slugified): `decisions` → Decision; `bug-classes`, `issues`, `bugs` → Bugfix; `methods`, `runbook` → Howto; `status`, `progress` → Status; `diary` → Diary; `knowledge` → Fact; anything else → Note.
3. **Order inside `map` (A1, A2):** (a) remove NUL bytes, NFC; (b) trim-empty check; (c) `filter.filter("content"|"chunk", whole_text)` — on `Err(detectors)` return `Blocked`; (d) **only now** split by size; (e) re-check each part's byte length (a `[REDACTED:…]` marker can be longer than the secret) and re-split a part that grew past the limit; (f) every other string the planner emits goes through `filter` too: `title`, each topic, `source.uri`, `source.session_id`, `source.agent`, `author.agent` (a blocked field blocks the drawer). Block detail names detectors only.
4. Knowledge memory: `topics` = [room slug] (diary: [`topic` slug] when present, else none); `importance` 3 (diary 2); `author.agent` = `meta.agent` (diary) else `added_by` unless it is `mempalace`, else the `Agent` wing suffix; `created_at` = `parse_time(filed_at)` else `parse_time(date)` else none; `legacy_id` = drawer id; `title` = `None` (derived from the first line) except diary: `format!("Diary {date}: {topic}")` when both exist (≤ 200 chars). Text over 65,536 bytes is split at paragraph (`\n\n`), else line, else char boundary into parts of ≤ 60,000 bytes; part *k* (k ≥ 2) has `legacy_id` `"<id>#<k>"` and title suffix ` (part k/n)`.
5. Raw chunk: `source` for `RawSession` = type Session, `uri` `file://<source_file>` (absolute paths) else the string as is; no `source_file` → `mempalace-wing:<wing>`; `agent` = `claude-code` if the path contains `/.claude/`, `codex` if it contains `/.codex/`, else none; `session_id` = file stem; `started_at`/`ended_at` from the `span` argument; `RawFile` = type File, same uri rule. `chunk.ord` = the `ord` argument; `chunk.authored_at` = `parse_time(authored_at)` (RawFile: `parse_time(filed_at)`); `chunk.legacy_id` = drawer id; project from `wings.decide`. Text over 32,768 bytes is split into ≤ 32,000-byte pieces; piece *k* ≥ 2 has `ord + k*10_000_000` and `legacy_id` `"<id>#<k>"`.
6. `parse_time`: RFC 3339 with `Z` or an offset; naive `YYYY-MM-DDTHH:MM:SS[.ffffff]` interpreted in `assume` (MemPalace writes naive local times); bare `YYYY-MM-DD` (midnight in `assume`); anything else → `None`. Output `…T…​.mmmZ`.

- [ ] **Step 1: Write the failing tests** (helper `d(id, text, meta)` builds `Drawer`s; `NoFilter` unless stated)

```rust
#[test]
fn classification_follows_the_real_palace_shapes() { /* the six assertions of the previous revision, unchanged:
   convos jsonl → RawSession; diary → Knowledge; agent-written without source → Knowledge; file chunk with added_by agent → RawFile ("added_by alone is not knowledge");
   curated room + source_file + agent → Knowledge; registry → Skip("MemPalace registry entry"); and new: meta.unreadable → Skip("unreadable metadata") */ }

#[test]
fn rooms_become_kinds_and_topics() { /* decisions→Decision, Bug_Classes→Bugfix, runbook→Howto, progress→Status, architecture→Note */ }

#[test]
fn a_diary_drawer_becomes_a_diary_memory_with_its_agent_topic_and_date() {
    /* meta: room diary, agent claude-code, topic "system-maintenance", date 2026-04-16, filed_at 2026-04-16T11:34:59.427116, wing wing_claude_code, FixedOffset::east(7*3600)
       → Planned::Memories(1 part): kind Diary, title "Diary 2026-04-16: system-maintenance", topics ["system-maintenance"], importance 2,
         author.agent Some("claude-code"), created_at Some("2026-04-16T04:34:59.427Z"), legacy_id = the drawer id */ }

#[test]
fn naive_and_zoned_times_parse_and_junk_is_none() { /* the five parse_time assertions of the previous revision (UTC+7 offset) */ }

#[test]
fn a_secret_straddling_the_split_point_is_redacted_before_splitting() {
    let secret = concat!("gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789");
    // text = 31,990 filler bytes + secret + 2,000 filler bytes: the secret crosses the 32,000-byte piece boundary
    let text = format!("{}{}{}", "a ".repeat(15_995), secret, " b".repeat(1_000));
    let f = RedactingFilter::default();                     // test filter: replaces ghp_… by "[REDACTED:github_token]", Err for text containing "BLOCKME"
    let m = Mapper { wings: &wings(), assume: utc(), filter: &f };
    let Planned::Chunks(parts) = m.map(&raw_drawer("d1", &text), 0, (None, None)) else { panic!() };
    assert!(parts.len() >= 2);
    let all: String = parts.iter().map(|p| p.chunk.text.as_str()).collect::<Vec<_>>().join("");
    assert!(!all.contains("ghp_") && all.contains("[REDACTED:github_token]"));
    assert!(parts.iter().all(|p| p.chunk.text.len() <= 32_768));
}

#[test]
fn a_redaction_marker_longer_than_the_secret_cannot_push_a_part_over_the_limit() {
    /* filter that replaces every "XX" with a 40-byte marker; a 32,000-byte text of "XX" pairs → after redaction > 32,768 → mapper re-splits; every part ≤ 32,768 */ }

#[test]
fn fields_other_than_the_text_are_filtered_too() {
    /* filter blocks any string containing "BLOCKME": source_file "/h/BLOCKME/s.jsonl" → Planned::Blocked{..}; topic "BLOCKME" (room) → Blocked; added_by "BLOCKME" → Blocked */ }

#[test]
fn oversize_empty_nul_and_decomposed_thai_are_handled_never_an_error() {
    /* 130 KB knowledge text → Memories with 3 parts, ids "<id>", "<id>#2", "<id>#3", each ≤ 60_000 bytes, titles end "(part 2/3)";
       40 KB chunk → 2 chunks, second ord = ord + 10_000_000; "   \n\t" → Skipped("empty text"); "a\0b" → content "ab";
       decomposed sara am "น\u{0e4d}\u{0e32}" → NFC "นำ" */ }

#[test]
fn split_utf8_never_cuts_a_character_and_loses_nothing() {
    /* Thai text of 100,000 bytes with max 32,000 → every part ≤ 32,000, concatenation == input, every part is valid UTF-8 (String guarantees) and ends at a prefer-separator when one exists in range */ }
```

- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-core import::plan -j 4`. Expected: FAIL.
- [ ] **Step 3: Implement** the rules. `local_offset()` = the machine's current UTC offset as a `chrono::FixedOffset` (chrono's `clock` feature is on).
- [ ] **Step 4: Run the tests** — PASS; `nice -n 15 cargo test -p alice-core -j 4` all green.
- [ ] **Step 5: Commit** — `git add crates/alice-core/src/import/plan.rs crates/alice-core/src/import/mod.rs && git commit -m "feat: classify and map single MemPalace drawers"` (trailer as in Global Constraints).

---

### Task 6: The survey and the planner

**Why:** exact-duplicate collapse and stable chunk positions need to see the whole palace once. The survey is cheap (ids, metadata, a hash per raw text), the planner then turns each drawer into writes. Chunk positions (`ord`) must not collide with chunks stored by an earlier run (A12).

**Files:**
- Modify: `crates/alice-core/src/import/plan.rs` (add `Survey`, `survey`, `Planner`), `crates/alice-core/src/import/mod.rs`
- Test: `crates/alice-core/src/import/plan.rs` (`mod tests`)

**Interfaces:**
- Consumes: Task 5 (`classify`, `Mapper`, `Planned`, `parse_time`), Task 2 (`PalaceReader`), Task 4 (`WingMap::note_transcript_dir`).
- Produces:

```rust
#[derive(Debug, Default)]
pub struct Survey {
    pub total: u64,
    pub wing_counts: Vec<(String, u64)>,
    pub knowledge_order: Vec<String>,                                   // knowledge drawer ids, earliest filed first
    pub ords: std::collections::HashMap<String, i64>,                   // raw drawer id → ord inside its source
    pub collapsed: std::collections::HashMap<String, String>,           // raw drawer id → keeper drawer id
    pub source_spans: std::collections::HashMap<String, (Option<String>, Option<String>)>, // uri → (first, last authored_at)
    pub skipped: std::collections::BTreeMap<&'static str, u64>,
    pub tiers: TierCounts,                                              // knowledge, raw_session, raw_file
    pub raw_source_of: std::collections::HashMap<String, String>,       // raw drawer id → source uri
}
/// `reserved(uri)`: the ords already stored for that source by an earlier run (a lazy database lookup supplied by the caller; `|_| BTreeSet::new()` for a first run and in pure tests).
pub fn survey(reader: &dyn PalaceReader, since: Option<&str>, wings: &mut WingMap, assume: FixedOffset,
              reserved: &dyn Fn(&str) -> std::collections::BTreeSet<i64>) -> Result<Survey>;

pub struct Planner<'a> { pub mapper: Mapper<'a>, pub survey: &'a Survey }
impl Planner<'_> { pub fn plan(&self, d: &Drawer) -> Planned; }   // Collapsed if the survey collapsed it; else Mapper::map with the survey's ord and span
```

**Rules (pinned by tests):**
1. `survey` pages the whole reader (5,000 per page, `since` applied), classifies each drawer, hashes raw text (first 16 bytes of SHA-256 of the NUL-stripped NFC text), calls `wings.note_transcript_dir` for transcript paths, records the knowledge order `(filed_at, id)` and per-source first/last authored time.
2. **Collapse:** within one source, rows with the same text hash collapse into the earliest by `(chunk_index.unwrap_or(i64::MAX), filed_at, id)`.
3. **Ords (A12):** per source, `claimed = reserved(uri)` initially; process the non-collapsed rows sorted by `(chunk_index.unwrap_or(i64::MAX), filed_at, id)`: a row whose `chunk_index` is ≥ 0 and not in `claimed` takes it (and joins `claimed`); every other row is deferred; deferred rows get `max(claimed) + 1, +2, …` in order. A drawer already imported by an earlier run keeps its stored ord (`reserved` reports its stored ord, and the importer never re-inserts it: Task 10 filters by drawer id first), so a later run can never assign an ord an earlier row holds.
4. `reserved` is called once per source that has at least one new row, never for the others.
5. Memory: ≈ 150–250 bytes per raw drawer in the maps (≈ 30 MB for the owner's palace, ≈ 200 MB at 1M); the importer records its peak RSS in the report.

- [ ] **Step 1: Write the failing tests** (the fixture feeds `survey` through `build_sqlite_exact` + `snapshot` + `open_snapshot`)

```rust
#[test]
fn survey_assigns_unique_ords_and_collapses_identical_chunks_in_one_source() {
    /* source A = /h/.claude/projects/-Users-me-Code-pg-ai/a.jsonl, drawers (id, chunk_index, text): a0 (0,"a"), a1 (1,"b"), a2 (1,"c"), a3 (2,"a"), a4 (none,"d")
       expected: ords {a0→0, a1→1, a2→3, a4→4}; a3 collapsed into a0; all ords unique */ }

#[test]
fn a_second_run_never_reuses_an_ord_an_earlier_run_stored() {
    /* run 1 over drawers a0 (0), a1 (none) → a1 gets ord 1.   Run 2 (reserved = |_| {0,1}) over the NEW drawers only (--since) a2 (chunk_index 1, text "new"), a3 (none) →
       a2's wanted index 1 is claimed → deferred → ord 2; a3 → ord 3; no ord equals 0 or 1 */ }

#[test]
fn knowledge_order_is_earliest_filed_first_and_ties_break_by_id() { /* 3 knowledge drawers with filed_at 2026-02, 2026-01, 2026-01 (ids b, a) → order [a, b, first-feb] */ }

#[test]
fn counts_tiers_skips_and_unknown_transcript_folders() {
    /* fixture of 1 registry drawer, 2 diary, 3 jsonl chunks in -Users-me-Code-AI-gsb-app-verify (not a known wing), 1 file chunk, 1 unreadable-metadata drawer
       → tiers {knowledge 2, raw_session 3, raw_file 1}, skipped {"MemPalace registry entry": 1, "unreadable metadata": 1}, wings.projects() now lists "gsb-app-verify" */ }

#[test]
fn since_limits_the_survey() { /* survey(.., Some("2026-03-01"), ..) counts only newer drawers; total reflects the filtered set */ }

#[test]
fn the_planner_returns_collapsed_for_collapsed_drawers_and_maps_the_rest() { /* a3 → Planned::Collapsed{keeper:"a0"}; a0 → Chunks with ord 0 and source.started_at/ended_at from the span */ }
```

- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-core import::plan -j 4`. Expected: FAIL.
- [ ] **Step 3: Implement** per the rules.
- [ ] **Step 4: Run the tests** — PASS; `nice -n 15 cargo test -p alice-core -j 4` green.
- [ ] **Step 5: Commit** — `git add crates/alice-core/src/import && git commit -m "feat: survey MemPalace drawers for duplicates and stable chunk positions"`

---

### Task 7: Knowledge graph, predicates and tunnels

**Files:**
- Create: `crates/alice-core/src/import/kg.rs`
- Modify: `crates/alice-core/src/import/mod.rs` (`pub mod kg;`), `crates/alice-core/src/import/fixture.rs` (`build_kg`, `build_tunnels`)
- Test: `crates/alice-core/src/import/kg.rs` (`mod tests`)

**Interfaces:**
- Consumes: `Snapshot { kg, tunnels }` (Task 2), `plan::{parse_time, TextFilter}` (Tasks 5), `store::kg::{AssertFact, FactAction, ObjectKind, normalize_predicate}`, `judge::text::predicate_similarity`, `store::reviews::{NewReview, ReviewType}`.
- Produces:

```rust
pub struct KgTriple { pub id: String, pub subject: String, pub predicate: String, pub object: String,
                      pub valid_from: Option<String>, pub valid_to: Option<String>,
                      pub extracted_at: Option<String>, pub source_drawer_id: Option<String> }
pub struct Kg { pub triples: Vec<KgTriple>, pub isolated_entities: u64, pub warnings: Vec<String> }
pub fn read_kg(path: &Path) -> Result<Kg>;            // opens read-only; entity ids resolved to names
pub struct PlannedFact { pub legacy_id: String, pub add: AssertFact, pub end_at: Option<String>, pub source_drawer_id: Option<String> }
pub struct KgPlan { pub facts: Vec<PlannedFact>, pub reviews: Vec<NewReview>, pub blocked: Vec<Vec<String>> /* detectors per blocked triple */, pub warnings: Vec<String> }
/// `existing`: (predicate name, how many relations use it) already in Alice's database.
pub fn plan_kg(kg: &Kg, existing: &[(String, u64)], assume: FixedOffset, filter: &dyn TextFilter) -> KgPlan;

pub struct Tunnel { pub id: Option<String>, pub label: Option<String>, pub kind: Option<String>,
                    pub source_wing: String, pub source_room: String, pub target_wing: String, pub target_room: String,
                    pub source_drawer_id: Option<String>, pub target_drawer_id: Option<String> }
pub struct Tunnels { pub explicit: Vec<Tunnel>, pub dropped_topic: u64, pub dropped_malformed: u64 }
pub fn read_tunnels(path: &Path) -> Tunnels;          // missing/corrupt file → empty, never an error
```

**Rules (A2, A5, A11, S10):**
- *Reading:* `SELECT t.id, s.name, t.predicate, o.name, t.valid_from, t.valid_to, t.source_drawer_id, t.extracted_at FROM triples t LEFT JOIN entities s ON s.id = t.subject LEFT JOIN entities o ON o.id = t.object ORDER BY t.id`; an old palace without `source_drawer_id` is handled via `PRAGMA table_info(triples)`; a triple whose entity row is missing uses the entity id as its name plus a warning. Entities that no triple mentions are counted in `isolated_entities` and **not imported** (Plan decision 21: an entity without a fact carries no knowledge; the report says how many).
- *Planning:* sort by `(subject, predicate, valid_from, id)`; each triple becomes `AssertFact { subject, predicate, object, object_kind: Auto, valid_from: parse_time(valid_from or extracted_at), action: Add, source_memory_id: None, author: Author{agent: Some("mempalace-import")} }`; a parsing `valid_to` gives `end_at`. `legacy_id = "kg:<triple id>"`. **Every string** (subject, predicate, object) goes through `filter`; `Err(detectors)` drops the triple into `blocked`.
- *Predicate reviews (follow M2 Task 13's convention — merge M2 first and read how it builds its predicate review; the shape below is what M2 consumes today and must be kept):* over the distinct `normalize_predicate` values of the KG plus `existing`, every unordered pair with `predicate_similarity ≥ 0.9` and different normal form yields `NewReview { kind: Predicate, payload: {"predicate": <other>, "canonical": <chosen>, "p": similarity, "provider": "mempalace-import"}, confidence: Some(similarity), dedupe_key: Some("predicate:<other>") }`. `canonical` = the predicate used by more relations (counting the KG's own triples plus `existing`); ties → the one already in Alice's vocabulary, then lexicographic order. **Never** merged automatically (spec §9; predicate similarity is direction-blind — `depends_on` vs `depended_on_by` — which is why this is only a review item). More than 5,000 distinct predicates skip the pairing with a warning.
- *Tunnels (A5):* MemPalace's file is `tunnels.json` in the palace folder's **parent** (`snapshot.tunnels`); a JSON array (or `{"tunnels": [...]}`) of `{"id","source":{"wing","room","drawer_id"?},"target":{...},"label","kind":"explicit"|"topic","created_at",…}`. `kind == "topic"` tunnels (derived from shared room names) are dropped and counted in `dropped_topic`; an element without both wing/room pairs is counted in `dropped_malformed`. The old flat form (`source_wing`, `source_room`, …) is accepted as a fallback.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn triples_read_with_entity_names_and_missing_entities_are_warned_not_fatal() {
    /* build_kg: entities e1 "alice", e2 "sqlite", e3 "postgres", e4 "lonely"; triples t1 (e1 uses e2), t2 (e1 uses e3, valid_to set), t3 (subject id "e9" missing, object e2)
       → 3 triples, names resolved; t3's subject == "e9" and warnings.len() == 1; isolated_entities == 1 ("lonely") */ }

#[test]
fn an_ended_triple_plans_an_add_and_an_end_time() { /* t2 → end_at Some, add.valid_from == parse_time(valid_from); t1 end_at None; legacy ids "kg:t1","kg:t2"; valid_from missing → extracted_at used */ }

#[test]
fn similar_predicates_become_review_items_in_the_m2_shape_never_merges() {
    /* predicates has_bug ×3 relations, had_bug ×1, had_critical_bug ×1, located_at ×2 → review for (had_bug → canonical has_bug) with payload keys predicate/canonical/p/provider and dedupe key "predicate:had_bug";
       located_at appears in none; plan.facts keep the ORIGINAL predicate text; ties pick the predicate already in `existing` */ }

#[test]
fn secrets_in_a_kg_object_block_that_triple_only() { /* filter blocks "BLOCKME" → the triple with object "token BLOCKME" is in plan.blocked, the others are planned */ }

#[test]
fn an_old_palace_kg_without_source_drawer_id_still_reads() { /* build_kg(legacy_schema=true) */ }

#[test]
fn tunnels_parse_mempalaces_real_shape_and_survive_garbage() {
    /* build_tunnels writes [{"id":"t1","source":{"wing":"pg_ai","room":"decisions","drawer_id":"d1"},"target":{"wing":"devspace","room":"src","drawer_id":"d2"},"label":"x","kind":"explicit"},
       {"id":"t2","source":{"wing":"a","room":"r"},"target":{"wing":"b","room":"r"},"kind":"topic"}, {"id":"t3"}] → explicit.len()==1, dropped_topic==1, dropped_malformed==1;
       the flat legacy form parses; a corrupt file and a missing file give empty Tunnels, never Err/panic */ }
```

- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-core import::kg -j 4`. Expected: FAIL.
- [ ] **Step 3: Implement** per the rules. `build_kg` writes `entities(id, name, type, properties, created_at)` and `triples(id, subject, predicate, object, valid_from, valid_to, confidence, source_closet, source_file, source_drawer_id, adapter_name, extracted_at)` exactly as `mempalace/knowledge_graph.py` creates them (legacy flag omits the last three).
- [ ] **Step 4: Run the tests** — PASS.
- [ ] **Step 5: Commit** — `git add crates/alice-core/src/import && git commit -m "feat: map the MemPalace knowledge graph and tunnels"`. **Finalise after M2 Task 13 is merged** (re-read its review payload; adapt the keys if it changed).

---

### Task 8: `WriteOp::ImportBatch`

**Why:** one bounded, atomic, idempotent write that the importer (Tasks 9–11) and nothing else uses. It keeps the writer's promise (short transactions) and gives resume for free: every legacy id is recorded with the data in the same transaction, so a batch is either fully there or fully absent.

**Prerequisites:** M2 Tasks 12 (judge pipeline, activation) and 14 (`SetMeta`, `ActivityGuard`, `reviews::was_rejected`) are merged; Task 1 is done.

**Files:**
- Modify: `crates/alice-core/src/store/import.rs` (add `ImportBatch`, `apply_batch`), `crates/alice-core/src/store/history.rs` (`undoable()`: `import.batch` and `import.compat_enabled` are never undoable), `crates/alice-daemon/src/writer.rs` (`WriteOp::ImportBatch`, `WriteOutcome::Imported`, `import_batch`), `crates/alice-daemon/src/service.rs` (a thin `pub(crate) async fn submit_import(&self, batch) -> Result<BatchOutcome>`; no logic)
- Test: `crates/alice-core/src/store/import.rs`, `crates/alice-core/src/store/history.rs`, `crates/alice-daemon/src/writer.rs` (`mod tests`, in the style of `blocking_submit_works_outside_the_runtime`)

**Interfaces:**
- Consumes: Task 1 (`legacy_put`, `legacy_get`, `legacy_existing`, placeholder projects), Tasks 6–7 (`PlannedMemory`/`PlannedChunk` fields, `PlannedFact`, `NewReview`), `store::{memories::prepare/insert, sources::ingest, kg::{assert_fact, EntityMatcher, link_mentions}, reviews::{create, was_rejected}, projects::{upsert_project, add_alias, resolve_slug}}`.
- Produces:

```rust
pub struct ImportProject { pub slug: String, pub aliases: Vec<String> }       // created with key "import:<slug>" if absent; "global" is the existing global project
pub struct ImportMemory { pub project_slug: String, pub input: NewMemoryInput }   // input.legacy_id and input.created_at are set
pub struct ImportSource { pub project_slug: String, pub source: SourceInput, pub chunks: Vec<ChunkInput> } // chunk.legacy_id set
pub struct ImportFact { pub legacy_id: String, pub add: AssertFact, pub end_at: Option<String>, pub source_drawer_id: Option<String> }
pub struct ImportBatch {
    pub projects: Vec<ImportProject>, pub memories: Vec<ImportMemory>, pub sources: Vec<ImportSource>,
    pub facts: Vec<ImportFact>, pub links: Vec<(String, String)>,        // (from drawer id, to drawer id) → relates_to
    pub collapsed: Vec<(String, String)>,                                // (collapsed drawer id, keeper drawer id)
    pub link_mentions: Vec<String>,                                      // drawer ids of imported memories whose entity mentions to (re)link
    pub reviews: Vec<NewReview>,
    pub meta: Vec<(String, String)>,                                     // written to the `meta` table (the final batch carries `import.mempalace.last`)
    pub job_id: String,
}
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchOutcome {
    pub memories_new: usize, pub memories_duplicate: usize,
    pub chunks_inserted: usize, pub chunks_skipped: usize, pub sources: usize,
    pub facts_added: usize, pub facts_ended: usize, pub facts_skipped: usize,
    pub links_made: usize, pub links_dropped: usize, pub reviews_created: usize, pub reviews_skipped: usize,
    pub collapsed_recorded: usize, pub projects_created: usize, pub mentions_linked: usize,
    pub deferred_collapsed: Vec<(String, String)>,   // keeper not stored yet (the importer sends collapsed pairs last, so normally empty)
    pub warnings: Vec<String>,                        // e.g. an alias that already points at another project
    pub errors: Vec<String>,                          // e.g. a chunk whose insert was ignored by an ord collision (never silent)
}
/// Runs inside `SAVEPOINT import_batch`; on any error rolls back to it and returns the error (nested savepoints are legal inside the writer's per-job savepoint).
pub fn apply_batch(conn: &Connection, b: &ImportBatch, hlc: &mut dyn FnMut() -> String) -> Result<BatchOutcome>;
/// Records and bytes the writer should charge to its per-commit budget.
impl ImportBatch { pub fn weight(&self) -> (usize, usize); }
// alice_daemon::writer
WriteOp::ImportBatch(Box<ImportBatch>)   →   WriteOutcome::Imported(BatchOutcome)
```

**Semantics (pinned by tests):**
1. *Projects:* ensure each (`upsert_project` with `ProjectRef { key: "import:<slug>", slug, name: slug, root: None, remote: None }`, skip `global`) and each alias (`add_alias`; an alias already pointing elsewhere is skipped and reported in `warnings`).
2. *Memories (A7):* if the `legacy_id` is already in `legacy_ids`, skip (counts as duplicate, nothing written); else `prepare` + `insert`; `InsertOutcome::New` records `legacy_put(legacy, "memory", id)` and then sets, in the same savepoint, **`status = 'active'`** and **`judge_state = 'skipped'`** on the row (imported knowledge is live at once and the M2 judge never picks it up; M3 owns this, no M2 change); `Duplicate{id}` records `legacy_put(legacy, "memory", existing_id)` and counts `memories_duplicate`.
3. *Sources/chunks (A12, A13):* chunks whose `legacy_id` is already stored are removed before `sources::ingest`; after ingest, **read back each chunk by legacy id** (`SELECT id FROM chunks WHERE source_id = ?1 AND legacy_id = ?2`); a chunk whose insert was ignored (an `ord` collision) has no row → an entry in `errors` (counted by the report, never silent) and no `legacy_ids` row; found chunks get `legacy_put(legacy, "chunk", id)`.
4. *Facts:* skip if `legacy_ids` has `legacy_id`; else `kg::assert_fact(add)` then, if `end_at`, an `End` action at that time; `source_drawer_id` resolved through `legacy_get` to a memory id → `source_memory_id`; `legacy_put(legacy, "relation", relation_id)`.
5. *Links:* both drawer ids must resolve to `memory` targets and differ, else `links_dropped`; `INSERT OR IGNORE INTO links(from_id,to_id,type='relates_to',created_by='import',created_at)`.
6. *Collapsed:* `legacy_put(collapsed_id, target_type, target_id)` where the target is whatever the keeper's `legacy_ids` row says; a keeper not yet stored → `deferred_collapsed`.
7. *Mentions:* for each drawer id in `link_mentions` resolved to a memory, run the entity matcher (`EntityMatcher::load(conn)` once per batch, `link_mentions`) — the importer sends these after the graph phase so entities exist.
8. *Reviews (A11):* skipped (`reviews_skipped`) when `reviews::was_rejected(conn, key)` is true (merge M2 Task 14; the owner already said no) or when `legacy_ids` already holds `review:<key>` (this importer queued it before and the owner resolved it); otherwise `reviews::create` (the dedupe key covers still-open items) and `legacy_put("review:<key>", "review", id)`.
9. *Meta:* each `(k, v)` is upserted into `meta`.
10. *Writer (A16):* `import_batch` charges `batch.weight()` (records, text bytes) to `st.chunk_budget` / `st.byte_budget` like `ingest_slice`; if it does not fit in what is left of the current group commit it returns `Applied::More(WriteOp::ImportBatch(batch), None)` so an `Ingest` slice and an `ImportBatch` never share one transaction (a batch alone in an empty commit always fits, as a slice with all its budget takes at least one chunk). It records **one** `import.batch` event (`Actor::Import`, `target_type "import"`, `target_id = job_id`, `after = {counts}`, never any text), sets `st.inserted = true` when anything was inserted so the embedding worker wakes.

- [ ] **Step 1: Write the failing tests** (store tests use the `conn()` helper as in Task 1; `sample_batch()` is defined once: project `pg-ai` with alias `wing-pg-ai`; memories `d1` (decision) and `d2` (note); one source `file:///h/s.jsonl` with chunks `c1`,`c2`,`c3` at ords 0,1,2)

```rust
#[test]
fn a_batch_creates_projects_aliases_active_memories_and_chunks_in_one_go() {
    let (_d, conn) = conn();
    let out = apply_batch(&conn, &sample_batch(), &mut hlc()).unwrap();
    assert_eq!((out.memories_new, out.chunks_inserted, out.sources, out.projects_created), (2, 3, 1, 1));
    let m: (String, String) = conn.query_row("SELECT judge_state, status FROM memories WHERE legacy_id='d1'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(m, ("skipped".into(), "active".into()), "live at once, never judged");
    assert!(legacy_get(&conn, "d1").unwrap().is_some() && legacy_get(&conn, "c2").unwrap().is_some());
    assert_eq!(projects::resolve_slug(&conn, "wing_pg_ai").unwrap().unwrap().slug, "pg-ai");
}

#[test]
fn applying_the_same_batch_twice_changes_nothing() { /* second apply: memories_new, chunks_inserted, facts_added, reviews_created all 0; counts 2 memories, 3 chunks */ }

#[test]
fn identical_memories_collapse_and_both_old_ids_resolve_to_the_survivor() { /* two ImportMemory (legacy "x1","x2") same project/kind/title/body → memories_new 1, memories_duplicate 1; legacy_get("x2").target_id == legacy_get("x1").target_id; one memories row */ }

#[test]
fn facts_keep_validity_and_resolve_their_source_drawer() { /* fact kg:t1 with end_at and source_drawer_id "d1" in the same batch → relation.valid_to set, source_memory_id == d1's memory id; legacy "kg:t1" → relation */ }

#[test]
fn links_need_both_ends_and_unresolved_ones_are_counted() { /* d1–d2 linked once; d1–"gone" dropped → links_dropped == 1; relates_to row has created_by 'import' */ }

#[test]
fn reviews_are_not_recreated_after_the_owner_rejected_or_resolved_them() { /* create predicate review key predicate:had_bug; resolve it (rejected) via the M2 store API; apply the same batch again → reviews_skipped == 1, no new open item; a never-seen key still creates one */ }

#[test]
fn a_failing_row_rolls_the_whole_batch_back_even_on_a_plain_connection() {
    /* a memory with 70_000-byte content makes prepare fail → Err; afterwards: no project, no other memory, no legacy_ids row (SAVEPOINT import_batch rolled back) */ }

#[test]
fn an_ignored_chunk_insert_is_reported_not_silent() { /* a chunk with ord 1 already stored under another legacy id → errors.len()==1 naming the legacy id; no legacy_ids row for it; the others are inserted */ }

#[test]
fn imported_memories_get_their_entity_mentions_when_asked() { /* entity "sqlite" exists; memory body mentions SQLite; batch with link_mentions ["d1"] → mentions row (memory,entity) exists, mentions_linked == 1 */ }
```
and in `history.rs` tests `import_events_are_never_undoable` (`alice undo <import.batch event>` answers a clear "not undoable" sentence, no review item); in `writer.rs` tests:

```rust
#[test]
fn the_import_op_goes_through_the_writer_records_one_event_and_wakes_embedding() {
    /* spawn_writer; submit_blocking(WriteOp::ImportBatch(Box::new(sample_batch()))) → Imported(out) with out.memories_new == 2;
       events: exactly one row action 'import.batch', actor 'import', after_json does not contain the body text; the wake channel received a message */ }
#[test]
fn an_import_batch_and_an_ingest_slice_never_share_one_group_commit() {
    /* enqueue an Ingest of 200 chunks and an ImportBatch of 200 records back to back: the writer's commit log (test hook already used by the budget tests) shows two separate transactions */ }
#[test]
fn an_import_batch_is_refused_while_the_database_is_read_only() { /* read_only.set(..) → Err(ReadOnly) */ }
```

- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-core store::import store::history -j 4` and `… -p alice-daemon writer:: -j 4`. Expected: FAIL.
- [ ] **Step 3: Implement.** `apply_batch` brackets its work with `SAVEPOINT import_batch` (RELEASE on success, ROLLBACK TO + RELEASE on error). Keep `ImportBatch` serde-free except `BatchOutcome`. Add `WriteOutcome::name()` arm `"Imported"`. Read `writer.rs` `run`/`apply` and the budget code (`chunk_budget`, `byte_budget`, `ingest_slice`) before writing `import_batch`. Struct literals of `NewMemoryInput` in tests use `..Default::default()` (M2 Task 12 added fields).
- [ ] **Step 4: Run the tests** — PASS, plus `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --lib -j 4`.
- [ ] **Step 5: Commit** — `git add crates/alice-core/src/store/import.rs crates/alice-core/src/store/history.rs crates/alice-daemon/src/writer.rs crates/alice-daemon/src/service.rs && git commit -m "feat: one bounded writer op for importing records"`

---

### Task 9: Import job I — request, report, preflight, lifecycle, snapshot → survey → dry run

**Why:** the job runs on its own thread (spec §2.1: "Workers: … import"), never on the writer or an async runtime thread. This task builds the frame — types, refusals, lifecycle, the read-only phases and the routes — so Tasks 10 and 11 only add phases.

**Prerequisites:** Tasks 1, 6, 7, 8 done; M2 Task 14 merged (`ActivityGuard`), M2 Task 13 merged (predicate convention).

**Files:**
- Create: `crates/alice-core/src/import/report.rs`, `crates/alice-daemon/src/importer.rs`, `crates/alice-daemon/tests/import_flow.rs`
- Modify: `crates/alice-core/src/import/mod.rs` (`ImportRequest`, `ImportState`, `ImportStatus`, `pub mod report;`), `crates/alice-daemon/src/lib.rs` (`pub mod importer;`), `crates/alice-daemon/src/service.rs` (`ServiceOptions::mempalace_home`, the import slot, `start_import`, `import_status`, shutdown join, start-up sweep), `crates/alice-daemon/src/http.rs` (two routes), `crates/alice-daemon/src/client.rs` (`start_import`, `import_status`), `crates/alice-daemon/tests/http_api.rs` (`every_route_is_served` lists the new routes; guard tests)
- Test: `crates/alice-core/src/import/report.rs` (rendering), `crates/alice-daemon/tests/import_flow.rs`

**Interfaces:**
- Consumes: Tasks 2–8; `Service::{readers, writer, settings, stop, redactors}` (read `service.rs` first: `start()`, `shutdown()` at about 617–650, the `ServiceOptions` defaults); `ActivityGuard` (M2 Task 14).
- Produces:

```rust
// alice_core::import
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ImportRequest {
    #[serde(default)] pub palace: Option<String>,        // folder; the CLI always sends an absolute path; None needs ServiceOptions::mempalace_home
    #[serde(default)] pub dry_run: bool,
    #[serde(default)] pub since: Option<String>,         // YYYY-MM-DD or RFC 3339
    #[serde(default)] pub map_wing: BTreeMap<String, String>,
    #[serde(default)] pub collection: Option<String>,    // default mempalace_drawers
    #[serde(default)] pub allow_remote_embedding: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)] #[serde(rename_all = "snake_case")]
pub enum ImportState { Idle, Running, Done, Failed }
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ImportStatus { pub state: ImportState, pub job_id: Option<String>, pub phase: String,
                          pub done: u64, pub total: u64, pub report: Option<ImportReport>, pub error: Option<String> }
// alice_core::import::report — all `pub`, Serialize + Deserialize + JsonSchema
pub struct ImportReport { job_id, dry_run, started_at, finished_at: Option<String>, palace: PalaceSummary,
    assumed_utc_offset: String, tiers: TierReport, projects: BTreeMap<String, ProjectCounts>,
    wing_map: BTreeMap<String, WingMapEntry>, duplicates: DuplicateCounts, facts: FactCounts, tunnels: TunnelCounts,
    drawers_seen: u64, drawers_accounted: u64, closets_dropped: u64, isolated_entities: u64,
    review_items: u64, already_imported: u64, skipped: BTreeMap<String, u64>, split: SplitCounts,
    blocked_by_redaction: BTreeMap<String, u64>, redacted_fields: u64, redaction_note: Option<String>,
    phases: Vec<PhaseTiming>, peak_rss_mb: Option<u64>, warnings: Vec<String>, errors: Vec<String> }
impl ImportReport { pub fn render_text(&self) -> String; }
// alice_daemon::service
impl Service {
    pub fn start_import(self: &Arc<Self>, req: ImportRequest) -> Result<ImportStatus>;   // synchronous refusals below, then the thread starts
    pub fn import_status(&self) -> ImportStatus;
}
// ServiceOptions { mempalace_home: Option<PathBuf> }   // default None; process_env() fills it from $HOME/.mempalace
// HTTP: POST /api/v1/import/mempalace (body ImportRequest) -> 202 ImportStatus; GET /api/v1/import/status -> ImportStatus
```

**Synchronous refusals (A4, A20), each one or two sentences naming the fix:** one import is already running (with its progress); `palace` is `None` and no `mempalace_home` ("no palace given; pass --palace <folder>"); the path is missing / not a palace (`detect_backend`'s sentences); `since` is not a date (checked before any file is created); `embedding.provider` is not `local` and `allow_remote_embedding` is false ("importing queues every chunk for <provider>, which would send the owner's history to it; pass --allow-remote-embedding if that is what you want").

**Lifecycle (A17, A18):** `Service` gets `import: Mutex<ImportSlot { status: Arc<Mutex<ImportStatus>>, handle: Option<JoinHandle<()>> }>`. The thread receives an `ImportEnv` bundle (`readers`, `writer`, `redactors`, `settings`, `stop`, `home`, an `ActivityGuard`, `Arc<Mutex<ImportStatus>>`) and **not** an `Arc<Service>` (no reference cycle). `Service::shutdown()` sets `stop`, joins the import thread first, then stops worker/writer. At `Service::start` the daemon deletes everything in `ALICE_HOME/import-tmp/` (no import can be running at start). The thread body runs under `catch_unwind` (precedent: `worker.rs`).

**Phases of this task (the rest in Tasks 10–11):**
1. `snapshot` — `snapshot(palace, <home>/import-tmp/<job>/, mempalace_home)` with a drop guard that removes the directory on success and failure; surface the snapshot `notes` as report warnings.
2. `survey` — `reader.wing_counts()`, `WingMap::build(counts, req.map_wing)`, `plan::survey(..)` with the lazy `reserved` lookup `|uri| SELECT c.ord FROM chunks c JOIN sources s ON s.id = c.source_id WHERE s.uri = ?` through the reader pool; record `closets_dropped` (count of `mempalace_closets` documents).
3. `dry-run counts` — with `dry_run`, fill the report from the survey plus the `legacy_existing` check (the already-imported count), without any writer call, settings write or meta write; the report is what a real run would add.
Progress: `done`/`total` count drawers (`survey.total`); `import_status()` is lock-cheap. Peak RSS (`ps`-free: `/proc` or `getrusage`) and per-phase seconds go into the report.

- [ ] **Step 1: Write the failing tests.** `import_flow.rs` defines `Flow` (**new**, modelled on `Srv` in `tests/http_api.rs`: a `Service` in a temp `ALICE_HOME`, fake embedder, `ServiceOptions::default()` — env-free, `mempalace_home: None`) and the fixture palace used by Tasks 9–11 (built with `alice_core::import::fixture::build_sqlite_exact` and friends):
  - wings `pg-ai`, `pg_ai`, `wing_pg_ai`, `devspace`, `sessions`, `wing_claude_code`;
  - knowledge drawers: `k_dec` (room `decisions`, wing `pg-ai`, text containing "decision about the v4 frame"), `k_dup_a` (wing `pg_ai`, room `knowledge`, text "Frames are framed protobuf") and `k_dup_b` (wing `wing_pg_ai`, same room and **identical text** — the knowledge duplicate), `k_diary` (room `diary`, agent `claude-code`, topic `setup`, date `2026-04-16`), `k_agent` (wing `wing_claude_code`, text mentioning `pg-ai` twice);
  - two transcripts `/h/.claude/projects/-Users-me-Code-pg-ai/s1.jsonl` and `…-Code-devspace/s2.jsonl` with 40 chunks each (chunk 7 of `s1` repeated once → one raw duplicate), one chunk containing the phrase "quickstart transcript phrase";
  - 30 `devspace` file chunks (`.go`, room `src`, `added_by` `claude`);
  - one registry drawer, one drawer with unreadable metadata JSON;
  - `knowledge_graph.sqlite3` with 4 triples (one with `valid_to`; predicates `has_bug` ×2, `had_bug`, `located_at`) and 1 isolated entity; `tunnels.json` in the palace folder's parent with one resolvable tunnel (`k_dec` ↔ `k_agent` drawer ids) and one without drawer ids;
  - Counts to pin: knowledge drawers 5 (4 unique after collapse), raw_session 80 (79 unique), raw_file 30, skipped 2, closets 1.

```rust
#[tokio::test]
async fn a_dry_run_reports_what_a_real_run_would_add_and_writes_nothing() {
    let f = Flow::new().await;
    let before = f.table_counts().await;                                  // every table incl. events, meta, sync_state
    let st = f.run_import(f.request().dry_run(true)).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.unwrap();
    assert_eq!((r.tiers.knowledge, r.tiers.raw_session_chunks, r.tiers.raw_file_chunks), (5, 80, 30));   // before collapse; collapse numbers below
    assert_eq!((r.duplicates.knowledge, r.duplicates.raw), (1, 1));
    assert_eq!(r.skipped["MemPalace registry entry"], 1);
    assert_eq!(r.skipped["unreadable metadata"], 1);
    assert_eq!(r.closets_dropped, 1);
    assert_eq!(f.table_counts().await, before, "a dry run writes nothing");
    assert!(!f.setting_bool("compat.mempalace").await, "and does not turn compat on");
    assert!(std::fs::read_dir(f.home().join("import-tmp")).map(|d| d.count()).unwrap_or(0) == 0, "snapshot removed");
    assert!(r.phases.iter().any(|p| p.phase == "survey"));
}

#[tokio::test]
async fn refusals_are_one_clear_sentence_each_and_create_no_files() {
    /* palace None (no mempalace_home) → Err containing "--palace"; "/nonexistent" → "does not exist"; since "last tuesday" → "--since"; a folder with a text file named sqlite_exact.sqlite3 → Failed/Err containing "damaged";
       embedding.provider = "ollama" (settings patch) → Err containing "--allow-remote-embedding", and Ok when allow_remote_embedding is true;
       in every case ALICE_HOME/import-tmp is absent or empty */ }

#[tokio::test]
async fn the_source_palace_is_byte_identical_afterwards_and_a_wal_palace_works() {
    /* hash of every file in the palace dir before/after a dry run, for build_sqlite_exact and for build_sqlite_exact_wal (only empty -shm/-wal may be new) */ }

#[tokio::test]
async fn a_second_start_while_one_runs_is_refused_with_the_progress() { /* use a fixture of 20k drawers; start; immediately start again → Err "already running" containing a phase name; wait Done */ }

#[tokio::test]
async fn shutdown_joins_the_import_thread_and_the_daemon_start_sweeps_import_tmp() {
    /* start a big dry-run import, call service.shutdown() → returns within 10 s, status Failed("daemon stopped…"), no import-tmp/<job> left;
       pre-create import-tmp/old-job/x then Service::start on that home → the directory is gone */ }

#[tokio::test]
async fn the_import_holds_the_activity_guard_so_maintenance_stays_off_the_writer() { /* during a running job service.activity().is_busy() (M2 Task 14's accessor) is true; after, false */ }
```
  and in `tests/http_api.rs`: `import_routes_need_the_token_host_and_origin_guards` (the three standard negative cases for both routes) and `every_route_is_served` lists `/api/v1/import/mempalace` and `/api/v1/import/status`; in `report.rs`:

```rust
#[test]
fn the_text_report_names_every_count_and_never_prints_blocked_text() {
    let r = sample_report();
    let t = r.render_text();
    for needle in ["Knowledge memories", "Raw session chunks", "Duplicates collapsed", "Already imported", "Closets dropped", "Wing → project map", "pg_ai → pg-ai", "Skipped", "Redaction", "Phases", "Peak memory"] { assert!(t.contains(needle), "{needle}\n{t}"); }
    assert!(!t.contains("ghp_"));
}
```
- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --test import_flow -j 4`. Expected: FAIL (no `start_import`).
- [ ] **Step 3: Implement** report types and the text layout:

```
MemPalace import — /path/to/palace (sqlite_exact, 119,179 drawers) [dry run]
  Knowledge memories ...... 5,058
  Raw session chunks ...... 76,126 in 130 transcripts
  Raw file chunks ......... 38,000
  Duplicates collapsed .... knowledge 5, raw 27
  Already imported ........ 0
  Closets dropped ......... 4,416
  Knowledge graph ......... 159 facts (2 ended), 4 predicate reviews, 12 entities without facts left out
  Tunnels ................. 3 linked, 1 dropped (no drawer ids), 5 topic tunnels ignored
  Skipped ................. 24 (MemPalace registry entry: 24)
  Split ................... 0 memories, 0 chunks (over the size limits)
  Redaction ............... 311 fields redacted, 2 drawers blocked (github_token: 2)   [privacy.redact is off: Alice redacted anyway]
  Projects: pg-ai 35,201 · games-platform 18,000 · … (N total)
  Wing → project map (302 wings):
    pg_ai → pg-ai (alias) · wing_pg_ai → pg-ai (alias) · wing_claude_opus_4.7 → inferred per drawer, agent claude-opus-4-7 · …
  Phases: snapshot 12 s · survey 41 s · …    Peak memory 410 MB
  Times assumed in UTC+07:00. Embeddings are computed in the background; search works now.
  Warnings: … / Errors: none
```
  then `importer.rs` (phases 1–3, lifecycle), the `Service` methods, routes, client calls.
- [ ] **Step 4: Run the tests** — PASS; `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon -j 4` all green; `pgrep -fl "alice daemon run"` empty.
- [ ] **Step 5: Commit** — `git add crates/alice-core/src/import crates/alice-daemon/src/importer.rs crates/alice-daemon/src/lib.rs crates/alice-daemon/src/service.rs crates/alice-daemon/src/http.rs crates/alice-daemon/src/client.rs crates/alice-daemon/tests/import_flow.rs crates/alice-daemon/tests/http_api.rs && git commit -m "feat: import job frame — preflight, lifecycle, snapshot, survey, dry run"`

---

### Task 10: Import job II — knowledge and raw phases

**Why:** the bulk of the work: turn drawers into bounded, atomic, redacted batches, skipping what is already stored without doing any work for it.

**Files:**
- Modify: `crates/alice-daemon/src/importer.rs`, `crates/alice-daemon/tests/import_flow.rs`
- Test: `crates/alice-daemon/tests/import_flow.rs`, unit tests in `importer.rs` (batcher)

**Interfaces:**
- Consumes: Tasks 5–9; `Redactors::for_root_always_on(None)`, `Redactor::check(field, text)`; `store::import::legacy_existing`.
- Produces: the `knowledge` and `raw` phases, `RedactFilter` (a `TextFilter` over the redactor; `Err(AliceError::Refused { detectors, .. })` → `Err(detectors)`), `Batcher` (private).

**Phases:**
- `knowledge` — groups of 100 ids from `survey.knowledge_order` → **prefilter by drawer id (`legacy_existing`) before anything else** (A12b: a no-op re-run does no redaction or planning) → `reader.get(ids)` → `Planner::plan` (redaction inside, Task 5 order) → batches.
- `raw` — page the reader by id (2,000 per page); **prefilter the page's ids with `legacy_existing` first**; plan the rest; `Collapsed` results go to a `collapsed` list kept for Task 11; group by `(project, source uri)`.
- **Batching (A13):** a batch closes at 200 records or 64 KB of text, whichever comes first, **only on drawer boundaries**: the parts of one drawer (`id`, `id#2`, `id#3`) are always in the same batch (a drawer larger than the budget gets a batch of its own). One `submit_blocking` per batch; the writer's group commit interleaves `remember`s between batches.
- **Redaction accounting:** `RedactFilter::changed` increments `redacted_fields`; `Planned::Blocked { detectors }` → `blocked_by_redaction[detector] += 1` per detector, the drawer counts as accounted, the import continues. The report's `redaction_note` says when `privacy.redact` is off ("Alice redacted anyway").
- **Dry run:** the same code with the submit replaced by counting.
- **Resume:** nothing stored is touched again; killing the daemon between batches and re-running finishes the rest (Task 16 proves it).

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_palace_imports_into_both_tiers_with_projects_topics_and_authors() {
    let f = Flow::new().await;
    let st = f.run_import(f.request()).await;
    assert_eq!(st.state, ImportState::Done, "{:?}", st.error);
    let r = st.report.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(f.project_slugs().await, vec!["devspace", "global", "pg-ai"]);       // pg_ai / wing_pg_ai folded into pg-ai
    assert_eq!(r.wing_map["wing_pg_ai"].to, "pg-ai");
    assert_eq!(r.wing_map["wing_claude_code"].via, "inferred");
    assert_eq!(f.count("memories").await, 4);                                         // 5 knowledge drawers, one duplicate pair collapsed
    assert_eq!(f.count("chunks").await, 79 + 30);
    assert_eq!(f.legacy_target("k_dup_a").await, f.legacy_target("k_dup_b").await);   // both old ids resolve to the survivor
    assert!(f.recall("decision about the v4 frame", false).await.contains("v4 frame"));
    assert!(f.recall("quickstart transcript phrase", true).await.contains("transcript phrase"));
    assert_eq!(f.diary_author().await.as_deref(), Some("claude-code"));
    assert_eq!(f.distinct("memories", "judge_state").await, vec!["skipped"]);
    assert_eq!(f.distinct("memories", "status").await, vec!["active"]);
    f.wait_embedded().await;                                                          // fake embedder: embed_backlog drains
    assert_eq!(f.health().await.embed_backlog, 0);
}

#[tokio::test]
async fn secrets_in_imported_history_never_reach_the_database_the_report_or_the_meta_marker() {
    let secret = concat!("gh", "p_", "abcdefghijklmnopqrstuvwxyz0123456789");
    // three drawers carry it: a transcript chunk (also with the secret inside the source path /h/.claude/projects/-Users-me-Code-pg-ai/<secret>.jsonl),
    // the diary drawer's text, and a devspace code chunk — plus one chunk whose text straddles the 32,000-byte split point
    /* import; for table in chunks, memories, events, review_items, meta, sources(uri, session_id): no row contains the secret (SQL LIKE on the literal built with concat!);
       the report JSON and render_text() do not contain it; the chunks hold "[REDACTED:" markers; r.redacted_fields >= 4 */ }

#[tokio::test]
async fn redaction_runs_even_when_the_privacy_setting_is_off_and_the_report_says_so() { /* patch privacy.redact=false; import; secret still absent; r.redaction_note is Some(..) */ }

#[tokio::test]
async fn block_mode_skips_the_drawer_names_the_detector_and_keeps_going() {
    /* patch privacy.redact_mode = block; import → Done, errors empty; blocked_by_redaction has the detector key with the number of secret drawers; other chunks are imported;
       drawers_seen == drawers_accounted */ }

#[tokio::test]
async fn running_again_imports_nothing_does_no_planning_work_and_new_drawers_are_picked_up() {
    /* second run: tiers 0, already_imported == first-run accounted total, table counts unchanged, r.redacted_fields == 0 and r.phases show knowledge+raw under 1 s (no redaction work for stored ids);
       add 2 drawers to the palace (one with chunk_index None, one with chunk_index equal to a previously auto-assigned ord) → third run imports exactly 2, ords unique per source, errors empty */ }

#[tokio::test]
async fn the_parts_of_one_oversize_drawer_always_share_a_batch() {
    /* unit test on Batcher: records [d1 (3 parts, 150 records worth), d2 (1)] with limit 200 → d1's parts in one batch, never split across two even if the 200 limit falls inside d1;
       an integration case: 130 KB knowledge drawer → three memories "id", "id#2", "id#3" all present after import; killing between parts is impossible because they are one transaction */ }

#[tokio::test]
async fn odd_drawers_are_counted_not_fatal() {
    /* drawers: empty text, "   \n\t", NUL-containing, 130 KB knowledge text, 40 KB chunk, missing wing, missing chunk_index, wing "---" → Done, errors empty;
       r.skipped["empty text"] == 2; r.split.memories == 1; r.split.chunks == 1; `SELECT max(length(CAST(body AS BLOB))) FROM memories` <= 65536; chunks <= 32768 */ }

#[tokio::test]
async fn an_ignored_chunk_insert_is_an_error_entry_never_silent() { /* force an ord collision (pre-store a chunk at the ord the planner will assign) → report.errors has one entry naming the legacy id, the exit is non-zero in the CLI (Task 12) */ }
```
- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --test import_flow -j 4`. Expected: FAIL.
- [ ] **Step 3: Implement** `RedactFilter`, `Batcher`, the two phases, counting. Redaction of project and alias names (A2) happens when the `ImportBatch.projects` list is built.
- [ ] **Step 4: Run the tests** — PASS; `pgrep -fl "alice daemon run"` empty.
- [ ] **Step 5: Commit** — `git add crates/alice-daemon/src/importer.rs crates/alice-daemon/tests/import_flow.rs && git commit -m "feat: import knowledge and raw drawers in atomic redacted batches"`

---

### Task 11: Import job III — graph, tunnels, collapsed, mentions; accounting; auto-enable; failure semantics

**Why:** the last phases, the proof that nothing was lost (the accounting law), the opt-in of the compat layer, and honest behaviour when things go wrong.

**Prerequisites:** M2 Task 13 merged (predicate review convention, `kg.rs` time guards) — re-read Task 7's note.

**Files:**
- Modify: `crates/alice-daemon/src/importer.rs`, `crates/alice-daemon/src/service.rs` (the settings patch after a real run), `crates/alice-daemon/tests/import_flow.rs`
- Test: `crates/alice-daemon/tests/import_flow.rs`

**Phases:**
- `graph` — `read_kg(snapshot.kg)`, existing predicates (`SELECT predicate, count(*) FROM relations GROUP BY 1`), `plan_kg` (filter = `RedactFilter`; blocked triples are counted per detector), tunnel links from `read_tunnels(snapshot.tunnels)` (explicit tunnels with both drawer ids → `links`, everything else counted in `tunnels.dropped`; topic tunnels in `tunnels.ignored_topic`), reviews; batched like the rest.
- `collapsed` — the collapsed-pairs list in batches of 500 (every keeper is stored by now).
- `mentions` — `link_mentions` for the imported knowledge drawer ids in groups of 200 (entities exist now).
- `done` — `drawers_accounted = imported memories + imported chunks (each drawer once, split parts counted once) + collapsed + skipped + blocked + already imported`; if it differs from `drawers_seen` push an `errors` entry naming both numbers. The final batch carries `meta: [("import.mempalace.last", {job_id, palace, finished_at, drawers_seen, ok})]` (no text, no secrets). Real runs only: if `compat.mempalace` is at its default (`SettingsView.sources["compat.mempalace"] == Source::Default`) call `patch_settings({"compat":{"mempalace":true}})` (the config follower records the `settings.change` event as it does for every patch) and submit `RecordEvent { actor: Import, action: "import.compat_enabled", target_type: "settings", target_id: "compat.mempalace", .. }`. A user who set `compat.mempalace = false` explicitly (source File) is left alone.
- **Failure semantics:** any error before the first batch → `Failed`, nothing written; an error or `ReadOnly` mid-way → `Failed("…run `alice import mempalace` again to continue; what was imported is kept")`; stop flag between batches → `Failed("daemon stopped; run the import again to continue")`; the snapshot directory is removed in every case.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn the_whole_fixture_imports_with_an_honest_report_and_the_accounting_law_holds() {
    let f = Flow::new().await;
    let r = f.run_import(f.request()).await.report.unwrap();
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(r.drawers_seen, r.drawers_accounted, "every drawer is imported, collapsed, skipped, blocked or already imported");
    assert_eq!(f.facts_timeline("alice").await.len(), 4);                         // timeline: the ended fact is included
    assert_eq!(r.facts.ended, 1);
    assert_eq!(r.review_items, 1);                                                // has_bug / had_bug, M2 payload {predicate, canonical, p, provider}
    assert_eq!(r.isolated_entities, 1);
    assert_eq!((r.tunnels.linked, r.tunnels.dropped), (1, 1));
    assert!(f.entity_mentions_exist().await);                                     // mention links made in the last phase
    assert_eq!(f.meta("import.mempalace.last").await.unwrap().contains("\"ok\":true"), true);
}

#[tokio::test]
async fn compat_turns_on_by_itself_once_and_respects_an_explicit_off() {
    /* after a real run: settings.compat.mempalace == true; events contain a settings.change naming compat.mempalace AND an import.compat_enabled row with actor "import";
       a second home where the user set compat.mempalace=false in config.toml first: after the run it is still false */ }

#[tokio::test]
async fn a_secret_in_a_kg_object_or_a_transcript_path_never_reaches_the_database() { /* triple object "key ghp_…" → blocked in block mode / redacted otherwise; relations and legacy_ids contain no secret */ }

#[tokio::test]
async fn a_read_only_database_mid_import_fails_with_the_resume_sentence() {
    /* after N batches set the writer read-only (ReadOnlyState::set) → status Failed with a sentence containing "run `alice import mempalace` again"; import-tmp empty; already stored rows intact */ }

#[tokio::test]
async fn re_review_items_resolved_by_the_owner_come_back_zero_times() { /* run; resolve the predicate review as rejected; run again → review_items 0 and open items 0 */ }
```
- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --test import_flow -j 4`. Expected: FAIL.
- [ ] **Step 3: Implement** the phases, the accounting, the settings step and the failure paths.
- [ ] **Step 4: Run the tests** — PASS; `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon -j 4`; `pgrep -fl "alice daemon run"` empty.
- [ ] **Step 5: Commit** — `git add crates/alice-daemon/src/importer.rs crates/alice-daemon/src/service.rs crates/alice-daemon/tests/import_flow.rs && git commit -m "feat: import graph, tunnels and duplicates; accounting law; compat opt-in"`

---

### Task 12: `alice import mempalace|status|snapshot`

**Files:**
- Modify: `crates/alice/src/main.rs` (clap subcommand `import`), `crates/alice/src/cmd.rs` (`import_mempalace`, `import_status`, `import_snapshot`)
- Test: `crates/alice/tests/cli.rs` (golden/snapshot tests in the file's existing style; the harness there is `Env` — read it first)

**Interfaces:**
- Consumes: `DaemonClient::start_import`/`import_status` (Task 9), `import::palace::snapshot` (Task 2), the CLI's existing "connect or start the daemon" helper (read `cmd.rs` for how `recall` does it).
- Produces: `alice import mempalace [--palace <path>] [--dry-run] [--since <date>] [--map-wing <wing>=<project>]... [--collection <name>] [--allow-remote-embedding] [--json]`, `alice import status [--json]`, and the **hidden** `alice import snapshot --palace <path> --out <dir>` (no daemon; runs `snapshot()` and prints the notes and the files written — the one audited way Task 19 copies the owner's palace).

Behaviour: the CLI resolves `--palace` to an absolute path (argument, else `MEMPALACE_PALACE_PATH`, else `MEMPAL_PALACE_PATH`, else `~/.mempalace/config.json` `palace_path` — read-only; else "no palace found; pass --palace <folder>") — **the daemon never resolves defaults (Global Constraints)**; `--map-wing` splits on the first `=`; start the job; poll `GET /import/status` every 500 ms; on a TTY print one rewritten progress line on stderr, otherwise a line per phase change; print `render_text()` (or the JSON with `--json`) on stdout. Exit code 0 only when the state is `done` and `report.errors` is empty; 1 otherwise with the failure sentence on stderr. Ctrl-C stops waiting only (the job continues; `alice import status` shows it; the next `alice import mempalace` resumes). A second import while one runs prints the daemon's sentence and the current progress. Help text: "Reads the palace through a read-only snapshot; MemPalace does not need to be installed or stopped. Run it again any time: it imports only what is new. Add --dry-run to see what would happen."

- [ ] **Step 1: Write the failing tests** (`Env` sets `HOME` to a temp dir and removes `MEMPALACE_PALACE_PATH`/`MEMPAL_PALACE_PATH`, `ALICE_EMBEDDER=fake`, `ALICE_JUDGE_PROVIDER=heuristic`; palace folders are built with `alice_core::import::fixture`, a dev-dependency with feature `fixtures`)

```rust
#[test]
fn import_dry_run_prints_the_report_and_changes_nothing() {
    let e = Env::new(); let palace = e.fixture_palace();
    let out = e.alice(&["import", "mempalace", "--dry-run", "--palace", palace.to_str().unwrap()]);
    assert!(out.status.success(), "{}", out.stderr);
    for needle in ["MemPalace import", "[dry run]", "Knowledge memories", "Wing → project map"] { assert!(out.stdout.contains(needle), "{needle}\n{}", out.stdout); }
    assert_eq!(e.alice(&["recall", "decision about the v4 frame"]).stdout.matches("v4 frame").count(), 0, "nothing was imported");
}

#[test]
fn import_then_recall_finds_the_old_memory_and_json_is_valid() { /* real run --json: parse stdout as ImportReport; errors empty; `alice recall "decision about the v4 frame"` finds it; `alice import status --json` says done; second run exits 0 with tiers all 0 */ }

#[test]
fn bad_inputs_exit_nonzero_with_one_sentence_on_stderr() {
    /* --palace /nonexistent → status 1, stderr contains "does not exist"; --since nope → stderr contains "--since"; --map-wing nope (no '=') → usage error, status 2;
       no --palace and HOME without ~/.mempalace → status 1, stderr contains "--palace" and no daemon was started */ }

#[test]
fn map_wing_overrides_reach_the_daemon() { /* --map-wing wing_project_alice=alice --json → report.wing_map["wing_project_alice"].to == "alice" */ }

#[test]
fn a_remote_embedding_provider_needs_the_explicit_flag() { /* `alice config set embedding.provider ollama` → import exits 1 mentioning --allow-remote-embedding; with the flag it starts (use --dry-run to avoid work) */ }

#[test]
fn the_hidden_snapshot_command_copies_through_the_audited_path_and_leaves_the_source_alone() {
    /* alice import snapshot --palace <wal fixture> --out <dir> → exit 0, <dir>/sqlite_exact.sqlite3 opens and counts the drawers; the source's main file hash is unchanged; the command appears neither in --help nor in the README */ }
```
- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-memory --test cli import -j 4` (copy the exact `-p` from `.github/workflows/ci.yml`). Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests** — PASS; `pgrep -fl "alice daemon run"` empty.
- [ ] **Step 5: Commit** — `git add crates/alice/src/main.rs crates/alice/src/cmd.rs crates/alice/tests/cli.rs && git commit -m "feat: alice import mempalace, status and the audited snapshot command"`

---

### Task 13: Compat core I — the read tools

**Why:** spec §8.2: existing prompts and hooks that call `mempalace_*` keep working against Alice. This task builds the translation frame and the read-only half, independent of MCP, so it is testable with plain function calls and reusable by HTTP and the MCP adapter (Task 15).

**Files:**
- Create: `crates/alice-daemon/src/compat.rs`, `crates/alice-core/src/store/compat.rs` (read-only queries), `crates/alice-daemon/tests/compat_flow.rs`
- Modify: `crates/alice-core/src/store/mod.rs` (`pub mod compat;`), `crates/alice-daemon/src/lib.rs`, `crates/alice-daemon/src/service.rs` (`compat_call`, a 5-second cache of the `status` overview), `crates/alice-daemon/src/http.rs` (`POST /api/v1/compat/call`), `crates/alice-daemon/src/client.rs` (`DaemonClient::compat_call`), `crates/alice-daemon/tests/http_api.rs` (`every_route_is_served` and the three guard cases for the new route)
- Test: `crates/alice-daemon/tests/compat_flow.rs` (reuses `Flow` and the fixture palace of Task 9, **including its knowledge duplicate pair** `k_dup_a`/`k_dup_b`, imported by `Flow::imported()`)

**Interfaces:**
- Consumes: `Service::{recall, get, context, projects}`, `store::import::legacy_get`, `store::projects::resolve_slug`, `normalize::{slugify, normalize_topic}`, `plan::kind_for_room` (Task 5).
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
pub enum CompatDrawer { Memory(String /*memory id*/), Chunk { id: String, project_slug: String, text: String, source_uri: String, ord: i64, created_at: String } }
pub fn resolve_drawer(conn: &Connection, drawer_id: &str) -> Result<Option<CompatDrawer>>;  // Alice id, legacy id (legacy_ids, memories.legacy_id), or chunk id
/// What `Hit` does not carry (A15): topics (first = room), source uri and ord for raw hits, legacy id; one bounded query per tier.
pub struct HitDetail { pub topics: Vec<String>, pub source_uri: Option<String>, pub ord: Option<i64>, pub legacy_id: Option<String> }
pub fn hit_details(conn: &Connection, memory_ids: &[String], chunk_ids: &[String]) -> Result<HashMap<String, HitDetail>>;
```
(`CompatDrawer` is named apart from `import::palace::Drawer`.)

**Exact output shapes** come from MemPalace's own source (read-only, installed at `/Users/icue/.venv/lib/python3.13/site-packages/mempalace/mcp_server/`): `tools_read.py` (`tool_status`, `tool_list_wings`, `tool_list_rooms`, `tool_search`), `tools_write.py` (`tool_get_drawer`), and the argument schemas in `schemas.py`. The implementer reads those functions and mirrors **every top-level key MemPalace returns** (keys Alice cannot fill get a neutral value: `0`, `""`, `[]`, `{}`), then adds `"alice": true` to `status` and `"note"` where behaviour differs. Known shapes: `status` → `{total_drawers, wings{}, rooms{}, protocol, aaak_dialect, backend}` (+ `context` text, `wing_project_map`); `list_wings` → `{wings: {name: count}}`.

**Mapping rules (pinned by tests):**
- *wing → project:* `projects::resolve_slug(wing)` (aliases such as `wing_pg_ai`, `pg_ai` resolve). Unknown wing on a read: empty result plus `"note": "no project or alias named <wing>"`.
- *search:* `RecallRequest { query, project: <wing>, topics: [room], since, before, include_raw: true, format: Json, limit: args.limit (default 5, ≤ 50), max_tokens: 6000 }`; for each hit use `hit_details` (A15) to build `{text: body-or-snippet, wing: project, room: first topic or "", source_file: source uri or "", similarity: score ÷ best score (3 decimals, 1.0 for the best), drawer_id: legacy id if the hit has one, else the Alice id, created_at, matched}`; `max_distance`, `candidate_strategy`, `cli_compatible`, `context` are accepted and ignored (named in a `"note"`). With `room` set only the knowledge tier can match (chunks carry no topics); the note says so.
- *status:* `overview()` through a 5-second cache (old prompts call `status` first in every session; at 1M chunks the per-project count must not be paid each time); serialized length < 6,000 bytes.
- *get_drawer:* through `Service::get` for memories (so M2's judge-duplicate redirect applies); a raw chunk returns `{drawer_id, content, wing, room: "", metadata{source_file, chunk_index}}`.
- *HTTP:* `POST /api/v1/compat/call` while `compat.mempalace` is false answers HTTP 409 with `{"error": "MemPalace compatibility is off; turn it on with `alice config set compat.mempalace true`"}` (same rule as MCP). Token, Host and Origin guards as every route.
- *argument coercion:* a missing required argument → `{"error": "missing required argument: query"}`; numbers sent as strings (`"limit": "5"`) and booleans as strings are accepted; `arguments` given as a JSON string holding an object is parsed once; wrong types give `{"error": "argument <name> must be a <type>"}`. Never a panic, never a protocol error.
- *unsupported tools:* any other `mempalace_*` name answers `{"supported": false, "tool": <name>, "message": "<what Alice does instead>"}` (a normal result, not an error) using this table: `mempalace_mine`, `mempalace_sync`, `mempalace_delete_by_source`, `mempalace_checkpoint` → "Alice captures sessions through its agent hooks and imports old palaces with `alice import mempalace`; save a memory with `remember`."; `mempalace_check_duplicate` → "`remember` detects exact duplicates itself (status duplicate); use `recall` to look for similar memories."; `mempalace_list_drawers` → "use `recall` with `topics`, `since` and `before`."; `mempalace_get_taxonomy` → "use `mempalace_list_wings` and `mempalace_list_rooms`, or the `context` tool."; `mempalace_kg_stats`, `mempalace_graph_stats`, `mempalace_memories_filed_away` → "the admin `stats` tool (mcp.admin_tools) and `alice status` show counts."; `mempalace_traverse`, `mempalace_find_tunnels`, `mempalace_create_tunnel`, `mempalace_list_tunnels`, `mempalace_delete_tunnel`, `mempalace_follow_tunnels`, `mempalace_list_hallways`, `mempalace_delete_hallway` → "Alice links memories (`remember` with `supersedes`, `update_memory` supersede) and the graph view in the app shows connections; there are no tunnels."; `mempalace_hook_settings` → "use `alice config`."; `mempalace_get_aaak_spec` → "Alice does not use AAAK; memories are plain text."; `mempalace_mesh_peers`, `mempalace_reconnect`, `mempalace_event_*`, `mempalace_task_create`, `mempalace_artifact_*`, `mempalace_patch_submit` → "coordination features are not part of Alice; agents share memory through `remember` and `recall`."; any other `mempalace_*` → "Alice does not provide <name>. Its tools are recall, remember, context, get_memory, update_memory, query_facts and assert_fact.".

- [ ] **Step 1: Write the failing tests** (`call(tool, json!({…}))` is a helper on `Flow` returning `CompatResult.value`)

```rust
#[tokio::test]
async fn old_wing_names_resolve_through_aliases() {
    let f = Flow::imported().await;
    let r = f.call("mempalace_search", json!({"query": "v4 frame", "wing": "wing_pg_ai"})).await;
    assert!(r["results"].as_array().unwrap().iter().any(|x| x["text"].as_str().unwrap().contains("v4 frame")));
    assert_eq!((r["results"][0]["wing"].as_str(), r["results"][0]["similarity"].as_f64()), (Some("pg-ai"), Some(1.0)));
    let w = f.call("mempalace_list_wings", json!({})).await;
    assert!(w["wings"]["pg-ai"].as_u64().unwrap() > 0 && w["wings"].get("wing_pg_ai").is_none());
}

#[tokio::test]
async fn search_fills_room_and_source_file_for_knowledge_and_raw_hits() {
    /* search "decision about the v4 frame" → the knowledge hit has room "decisions" (first topic) and source_file ""; search "quickstart transcript phrase" → the raw hit has source_file ending "s1.jsonl" and room "";
       a `room: "decisions"` filter returns only the knowledge hit and the note says raw chunks carry no rooms; unknown wing → results [] + note */ }

#[tokio::test]
async fn list_rooms_gives_topics_of_a_project_and_an_empty_answer_with_a_note_for_an_unknown_wing() {
    /* list_rooms{wing:"pg_ai"} → rooms has "decisions" and "knowledge" with counts ≥ 1; list_rooms{} → all rooms; list_rooms{wing:"nope"} → rooms {} + note */ }

#[tokio::test]
async fn get_drawer_resolves_alice_ids_old_ids_collapsed_ids_and_raw_chunks() {
    /* get_drawer{drawer_id:"k_dup_a"} and {"k_dup_b"} → the SAME content and the same Alice id (the survivor); get_drawer with the Alice id works;
       a raw drawer id (the "quickstart" chunk's legacy id) → content with metadata.source_file ending s1.jsonl; unknown id → {"error": …} containing "not found" */ }

#[tokio::test]
async fn status_gives_counts_and_the_wing_map_and_stays_small() {
    /* status → total_drawers == memories + chunks, wings has "pg-ai", "alice": true, context text present, serialized length < 6000 bytes; a second call within 5 s does not re-run the overview query (counter on the cache) */ }

#[tokio::test]
async fn unsupported_tools_bad_arguments_and_numbers_as_strings_get_helpful_answers() {
    /* mempalace_mine → supported:false and message contains "alice import mempalace"; mempalace_totally_new → message lists Alice's tools;
       search without query → {"error":"missing required argument: query"}; search {"query": 42} → error mentions "query"; search {"query":"x","limit":"abc"} → error mentions "limit"; limit:"3" works;
       arguments given as the JSON string '{"query":"x"}' works; none returns Err(..) from compat_call */ }

#[tokio::test]
async fn the_http_route_refuses_while_compat_is_off_and_is_guarded() { /* compat off → 409 body contains "compat.mempalace"; on → 200; token/Host/Origin negatives in http_api.rs */ }
```
- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --test compat_flow -j 4`. Expected: FAIL.
- [ ] **Step 3: Implement** `store/compat.rs` (read-only, bounded; `overview` runs two `GROUP BY` queries and caps rooms at 200), then `compat.rs`: one function per tool (`async fn status(svc, args, ctx) -> Value`), an `Args` wrapper with `required_str`, `opt_str`, `opt_u64`, `opt_bool` doing the coercions, and the `UNSUPPORTED` table as a `match`. The write tools answer, until Task 14, `{"error": "not available yet"}` behind a `todo`-free `match` arm that Task 14 replaces — and the tests of this task do not call them.
- [ ] **Step 4: Run the tests** — PASS; `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon -j 4` green.
- [ ] **Step 5: Commit** — `git add crates/alice-core/src/store/compat.rs crates/alice-core/src/store/mod.rs crates/alice-daemon/src/compat.rs crates/alice-daemon/src/lib.rs crates/alice-daemon/src/service.rs crates/alice-daemon/src/http.rs crates/alice-daemon/src/client.rs crates/alice-daemon/tests/compat_flow.rs crates/alice-daemon/tests/http_api.rs && git commit -m "feat: MemPalace compatibility layer — read tools"`

---

### Task 14: Compat core II — the write tools

**Files:**
- Modify: `crates/alice-daemon/src/compat.rs`, `crates/alice-core/src/store/compat.rs` (`recent_diaries`), `crates/alice-daemon/tests/compat_flow.rs`
- Test: `crates/alice-daemon/tests/compat_flow.rs`

**Interfaces:**
- Consumes: Task 13, `Service::{remember, update, assert_fact, facts}`.
- Produces: `store::compat::recent_diaries(conn: &Connection, agent: &str, project: Option<&str>, limit: usize) -> Result<Vec<Memory>>` and the nine write-side tool functions.

**Exact output shapes** from `tools_write.py` (`tool_add_drawer`, `tool_update_drawer`, `tool_delete_drawer`), `tools_diary.py`, `tools_kg.py` — read them and mirror every top-level key.

**Mapping rules (pinned by tests):**
- *Unknown wing on a write* (`add_drawer`, `diary_write`): the write goes to the caller's current project (`cwd`) with `topics = [room]` and the answer carries `"note": "wing <w> is not an Alice project; saved in <project>"` (agents never create containers, spec §3.2).
- *add_drawer:* `remember(kind = kind_for_room(room), topics = [normalize_topic(room)], author.agent = added_by else the caller's agent, project = resolved wing)`; `source_file` is appended as a final line `Source: <path>` (Alice has no per-memory file field). Answer `{success: true, drawer_id, wing, room}`; a duplicate: `{success: true, reason: "already_exists", drawer_id}`.
- *diary_write:* `remember(kind = diary, content = entry|content, title = format!("Diary: {topic or 'general'}"), topics = [topic], author.agent = agent_name, project = wing or cwd project)`; *diary_read:* `recent_diaries(agent_name, wing, last_n default 10)` → `{agent, entries: [{date, timestamp, topic, content}], total, showing}`; unknown wing → empty entries + note.
- *update_drawer:* memory → `update{action: edit, body, topics}`; `wing` given (a move) → `{"error": "moving a memory to another project is not supported; remember it in the new project and forget this one"}`; a raw chunk → `{"error": "raw transcript chunks cannot be changed one by one; …"}`. *delete_drawer:* memory → `update{action: forget}`; raw chunk → the same kind of error.
- *kg:* `kg_query` → `facts(entity, as_of)` split into `active_facts` / `historical_facts` (`valid_to` null vs set), `direction` filters on whether the entity is subject or object; `kg_add` → `assert_fact(add)`, then `end` when `valid_to` is given; `kg_invalidate` → `end` at `ended` (default now); `kg_supersede` → `assert_fact(supersede)` with `new_object` at `at` (a mismatch with `old_object` is reported in `"note"`, not refused); `kg_timeline` → `facts(entity, timeline)` with `limit`/`offset` applied.

- [ ] **Step 1: Write the failing tests** (split by tool so a failure names it)

```rust
#[tokio::test]
async fn add_get_update_delete_round_trip_with_old_style_arguments() {
    let f = Flow::imported().await;
    let a = f.call("mempalace_add_drawer", json!({"wing": "pg_ai", "room": "decisions", "content": "We chose X because Y", "added_by": "codex"})).await;
    assert_eq!(a["success"], true);
    let id = a["drawer_id"].as_str().unwrap().to_string();
    let m = f.memory(&id).await;
    assert_eq!((m.kind, m.topics.clone(), m.author.agent.as_deref()), (Kind::Decision, vec!["decisions".to_string()], Some("codex")));
    assert!(f.call("mempalace_get_drawer", json!({"drawer_id": id})).await["content"].as_str().unwrap().contains("We chose X"));
    let again = f.call("mempalace_add_drawer", json!({"wing": "pg_ai", "room": "decisions", "content": "We chose X because Y"})).await;
    assert_eq!((again["reason"].as_str(), again["drawer_id"].as_str()), (Some("already_exists"), Some(id.as_str())));
    f.call("mempalace_update_drawer", json!({"drawer_id": id, "content": "We chose Z"})).await;
    assert!(f.call("mempalace_get_drawer", json!({"drawer_id": id})).await["content"].as_str().unwrap().contains("We chose Z"));
    f.call("mempalace_delete_drawer", json!({"drawer_id": id})).await;
    assert!(f.call("mempalace_get_drawer", json!({"drawer_id": id})).await.get("error").is_some() || f.is_forgotten(&id).await);
}

#[tokio::test]
async fn an_unknown_wing_on_a_write_files_into_the_current_project_with_a_note() { /* add_drawer wing "nope" with cwd = a fixture project → saved there, note names both */ }

#[tokio::test]
async fn updates_and_deletes_of_raw_chunks_and_moves_between_wings_are_refused_with_a_sentence() { /* the raw id → error containing "raw"; update_drawer{wing:"devspace"} → error containing "not supported" */ }

#[tokio::test]
async fn diary_write_and_read_round_trip_for_an_agent() { /* diary_write{agent_name:"claude-code", entry:"did X", topic:"setup"} then diary_read{agent_name:"claude-code", last_n:"3"} (string number) → entries newest first, total ≥ 1; agent "other" sees none; diary_read with unknown wing → empty + note */ }

#[tokio::test] async fn kg_add_and_query_show_an_active_fact() { /* kg_add alice uses sqlite valid_from 2026-01-01 → kg_query alice has it in active_facts */ }
#[tokio::test] async fn kg_supersede_closes_the_old_value() { /* supersede sqlite→postgres at 2026-06-01 → active has postgres, historical has sqlite */ }
#[tokio::test] async fn kg_invalidate_ends_the_fact() { /* invalidate at 2026-07-01 → active empty */ }
#[tokio::test] async fn kg_timeline_lists_entries_in_time_order_with_limit_and_offset() { /* 2 entries in order; limit "1" offset "1" (strings) picks the second */ }
#[tokio::test] async fn kg_add_with_valid_to_creates_an_ended_fact() { /* kg_add … valid_to 2026-03-01 → historical */ }
```
- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --test compat_flow -j 4`. Expected: FAIL.
- [ ] **Step 3: Implement** the nine tools; replace the placeholder arms from Task 13.
- [ ] **Step 4: Run the tests** — PASS; `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon -j 4` green.
- [ ] **Step 5: Commit** — `git add crates/alice-core/src/store/compat.rs crates/alice-daemon/src/compat.rs crates/alice-daemon/tests/compat_flow.rs && git commit -m "feat: MemPalace compatibility layer — write tools"`

---

### Task 15: Compat on MCP — schemas, the shared switches gate, stdio shim

**Prerequisite (satisfied):** M2 Task 11 is merged: `mcp.rs` has `ADMIN_TOOLS`, `AliceMcp::tool_allowed`, `Backend::admin_tools()`, `RemoteBackend::admin_tools()` (config.toml + `ALICE_*` via `config::load_effective`, cached 1 s, **no daemon round trip**) and hand-written `list_tools`/`call_tool`. Compat reuses that mechanism (A14); it does not fork it and it does not add a daemon call to the shim.

**Files:**
- Create: `crates/alice-daemon/src/compat_schema.rs`
- Modify: `crates/alice-daemon/src/mcp.rs` (generalise `admin_switch` into one cached `Switches { admin_tools: bool, compat_mempalace: bool }` read by the same `load_effective` call; `Backend::compat_mempalace()` mirrors `admin_tools()`; extend `tool_allowed`; route `mempalace_*` in `call_tool`; add the tools in `list_tools`; `Backend::compat_call`), `crates/alice-daemon/src/lib.rs`
- Test: `crates/alice-daemon/tests/compat_flow.rs` (MCP tests; reuse `Daemon`/`TestClient`/`http_transport` of `tests/mcp.rs`), `crates/alice/tests/cli.rs` (shim test), `crates/alice-daemon/tests/mcp.rs` (the existing admin-tool tests stay green and **unchanged**)

**Interfaces:**
- Consumes: Tasks 13–14 (`compat_call`, `SUPPORTED`, `is_compat_name`), the gate above, `settings.compat.mempalace`.
- Produces: `compat_schema::tools() -> Vec<rmcp::model::Tool>` — one per `SUPPORTED` name with a one-sentence description and an `inputSchema` listing exactly MemPalace's property names, types and required arguments (copied by hand into a Rust table; the planner's reading of `schemas.py`: `search` {query*, limit, wing, room, source_file, since, before, max_distance, candidate_strategy, cli_compatible, context}; `add_drawer` {wing*, room*, content*, source_file, added_by}; `status` {}; `list_wings` {}; `list_rooms` {wing}; `get_drawer` {drawer_id*}; `update_drawer` {drawer_id*, content, wing, room}; `delete_drawer` {drawer_id*}; `diary_write` {agent_name*, entry, topic, wing, content}; `diary_read` {agent_name*, last_n, wing}; `kg_query` {entity*, as_of, direction}; `kg_add` {subject*, predicate*, object*, valid_from, valid_to, source_closet, source_file, source_drawer_id}; `kg_invalidate` {subject*, predicate*, object*, ended}; `kg_supersede` {subject*, predicate*, old_object*, new_object*, at}; `kg_timeline` {entity, limit, offset}; `*` = required — re-verify each against `schemas.py`).

**Behaviour:** `tool_allowed(tool)` becomes `if is_compat_name(tool) { self.backend.compat_mempalace() } else { !ADMIN_TOOLS.contains(&tool) || self.backend.admin_tools() }` — M2's `mcp.admin_tools` behaviour stays identical (its tests unchanged and green). `list_tools` = core + (admin tools while `admin_tools`) + `compat_schema::tools()` while `compat_mempalace`, evaluated on every call (no restart). `call_tool` for a `mempalace_*` name: allowed → `Backend::compat_call` and the JSON value returned as text content (an `{"error": …}` value is a normal result, as MemPalace does); not allowed → the same "tool not found" error an unknown name gets, with the data sentence "MemPalace compatibility is off; turn it on with `alice config set compat.mempalace true`". An allowed-but-unsupported name (`mempalace_mine`) is **not listed** but **is answered** (Task 13's message). The caller's `clientInfo.name` becomes `CompatCall.agent`; the project root comes from the same `cwd` resolution as the core tools. For the stdio shim the switch comes from `config.toml` (the auto-enable of Task 11 writes it through `patch_settings`, so the shim sees it within the 1 s TTL). If rmcp offers a `tools/list_changed` notification, send it when the setting flips (daemon side); per-call evaluation is the required behaviour and the notification optional — say which in the ledger.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn compat_tools_appear_and_vanish_with_the_setting_without_a_restart() {
    let f = Flow::imported_with_mcp().await;                    // in-process daemon + rmcp reference client over Streamable HTTP
    let count = |t: Vec<String>| t.into_iter().filter(|n| n.starts_with("mempalace_")).count();
    f.set_compat(false).await;
    assert_eq!(count(f.mcp_tool_names().await), 0);
    f.set_compat(true).await;                                    // settings PATCH, as `alice config set` does
    let on = f.mcp_tool_names().await;
    assert_eq!(count(on.clone()), 15);
    for core in ["recall", "remember", "context", "get_memory", "update_memory", "query_facts", "assert_fact"] { assert!(on.contains(&core.to_string())); }
    f.set_compat(false).await;
    assert_eq!(count(f.mcp_tool_names().await), 0);
}

#[tokio::test]
async fn a_call_while_compat_is_off_is_refused_with_the_way_to_turn_it_on() { /* mempalace_search with compat off → tool error containing "compat.mempalace"; on → results */ }

#[tokio::test]
async fn unsupported_names_are_answered_but_not_listed() { /* compat on: list lacks mempalace_mine; calling it returns the "alice import mempalace" message as a normal result */ }

#[tokio::test]
async fn every_listed_schema_names_mempalaces_arguments() {
    /* for each tool in compat_schema::tools(): inputSchema.type == "object"; required == the table above; a call built from the schema's own properties succeeds for search/add_drawer/kg_add */ }

#[tokio::test]
async fn author_is_the_mcp_client_name_for_compat_writes() { /* add_drawer without added_by through a client named "codex" → author.agent "codex"; with added_by "claude" → "claude" */ }

#[tokio::test]
async fn the_admin_tools_switch_behaves_exactly_as_before() { /* the M2 tests for mcp.admin_tools (admin tools listed only while on, refused as unknown when off) pass unchanged; add: both switches independent (admin on, compat off → admin tools listed, mempalace_* not) */ }
```
and in `crates/alice/tests/cli.rs`: `the_stdio_shim_lists_compat_tools_when_the_config_has_them_on` (start the daemon, `alice config set compat.mempalace true`, speak MCP to `alice mcp` over stdio with the client the file already has; wrap the listing in the file's `eventually(...)` helper because of the shim's 1 s TTL; assert 15 `mempalace_*` names and one successful `mempalace_list_wings` call; assert the shim never connected to the daemon just to decide the list — the listing works with the daemon stopped, only the call needs it).

- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --test compat_flow mcp -j 4`. Expected: FAIL.
- [ ] **Step 3: Implement** by extending M2 Task 11's code path; keep the shim start-up budget (< 20 ms: nothing is read at start-up, the switches are read on the first `list_tools`/`call_tool`).
- [ ] **Step 4: Run the tests** — PASS; `nice -n 15 cargo test -p alice-daemon --test mcp -j 4` (the existing conformance suite) still green; `pgrep -fl "alice daemon run"` empty.
- [ ] **Step 5: Commit** — `git add crates/alice-daemon/src/compat_schema.rs crates/alice-daemon/src/mcp.rs crates/alice-daemon/src/lib.rs crates/alice-daemon/tests/compat_flow.rs crates/alice/tests/cli.rs && git commit -m "feat: expose the MemPalace tools over MCP while compat is on"`

---

### Task 16: Concurrency and crash safety during an import

**Why:** the whole reason Alice exists is that MemPalace writers block each other during a mine (spec §0.1 problem 1). Spec §10 makes "20 clients writing and searching while an import runs" and "`kill -9` during import" acceptance tests. Task 8's bounded, budget-charged batches are the mechanism; this task proves it.

**Prerequisite:** M2 Task 16 merged (HTTP embedders; the embedding worker's behaviour under load), Task 11 done.

**Files:**
- Create: `crates/alice-daemon/tests/import_concurrency.rs`
- Modify: `crates/alice/tests/cli.rs` (the kill test), `crates/alice-core/src/import/fixture.rs` (`synthetic_palace`), `crates/alice-daemon/src/importer.rs` (batch counters exposed through the status), `docs/benchmarks/m3.md` (create; numbers filled in Task 19)
- Test: both test files

**Interfaces:** Consumes the `Flow` of Task 9, `fixture::synthetic_palace(dir, drawers: usize)` (add it here: mostly transcript chunks of 600–900 chars of mixed Thai/English/code words, 3% knowledge drawers, 5% repeats), and the pattern of `crates/alice-daemon/tests/service_concurrency.rs` (read it first: it already measures `remember`/`recall` p95 and counts lost writes; copy its helpers).

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_clients_keep_working_while_a_palace_imports() {
    let n = std::env::var("ALICE_TEST_BULK").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000usize);
    let f = Flow::with_synthetic_palace(n).await;                  // fake embedder, temp home
    f.start_import_default();
    let stats = f.twenty_clients_until_import_done().await;        // 20 tasks: remember(unique text) + recall(random word), recording latency and every acknowledged id
    assert!(f.import_done_ok().await);
    assert_eq!(f.count_ids_present(&stats.acknowledged).await, stats.acknowledged.len());
    assert_eq!(f.duplicate_memory_hashes().await, 0);
    assert_eq!(f.chunk_count().await + f.collapsed_count().await, f.expected_chunks);
    if !cfg!(debug_assertions) {                                    // budgets enforced in release builds only, as in service_concurrency.rs
        assert!(stats.remember_p95_ms < 50.0, "remember p95 {} ms", stats.remember_p95_ms);
        assert!(stats.recall_p95_ms < 200.0, "recall p95 {} ms", stats.recall_p95_ms);
    }
    eprintln!("import {} drawers in {:?}; remember p95 {:.1} ms, recall p95 {:.1} ms", n, f.import_elapsed(), stats.remember_p95_ms, stats.recall_p95_ms);
}

#[tokio::test]
async fn one_batch_never_holds_the_writer_longer_than_the_slice_budget() {
    /* a task submits a remember every 5 ms while a 20k import runs; the longest ack (debug < 750 ms, release < 150 ms) proves no batch is long;
       the status exposes max batch records/bytes: ≤ 200 records and ≤ 64 KB; and an Ingest slice and an ImportBatch never shared a commit (writer commit log) */ }

#[tokio::test]
#[ignore = "real e5 model (linked from ~/.alice/models); the controller runs it"]
async fn recall_stays_within_budget_while_the_real_model_embeds_an_import() { /* 20k synthetic drawers with the real embedder; recall p95 < 200 ms (release); report the number */ }
```
```rust
// crates/alice/tests/cli.rs
#[test]
#[cfg(unix)]
fn kill_minus_nine_during_an_import_loses_nothing_and_the_rerun_finishes_exactly() {
    /* Env: throwaway home, fake embedder, a 30k-drawer synthetic palace.
       1. start `alice import mempalace --palace P --json` in the background; poll `alice import status --json` until done ≥ 3,000 drawers;
       2. send remember("acked before kill N") ×20, keep the ids; SIGKILL the daemon pid (daemon.json);
       3. any `alice` call restarts the daemon; ALICE_HOME/import-tmp has been swept (empty) ; run the import again to the end → exit 0, errors empty;
       4. every acknowledged id present; `PRAGMA quick_check` ok (open read-only AFTER stopping the daemon); memories + chunks + collapsed rows == the fixture's counts exactly;
          a third run reports tiers all 0 and already_imported == total; the palace directory hash is unchanged. */ }

#[test]
fn a_target_that_cannot_be_written_fails_cleanly_and_resumes() {
    /* make ALICE_HOME/import-tmp unwritable (unix: chmod 0555 on a pre-created import-tmp) → Failed with a sentence naming the folder; restore permissions → rerun succeeds */ }
```
- [ ] **Step 2: Run to verify they fail** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --test import_concurrency -j 4` and the cli tests. Expected: FAIL (missing helpers). Add `synthetic_palace` and the counters.
- [ ] **Step 3: Implement the helpers; fix what the tests find.** If the p95 assertions fail in `--release`, the fix is in the batching (smaller batches, a 2–5 ms pause between batches while the writer queue is non-empty) and **not** in relaxing a budget. Record what changed in the ledger.
- [ ] **Step 4: Run the tests** — debug: `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --test import_concurrency -j 4`; the release latency proof (one heavy run, alone, `timeout 20m`, reusing the release build of Task 19 if it exists): `ALICE_TEST_BULK=100000 env -u TYPESAFE_API_KEY nice -n 15 cargo test --release -p alice-daemon --test import_concurrency twenty_clients -j 4 -- --nocapture` → paste the printed line into `docs/benchmarks/m3.md` under "Import under load". Kill test: `nice -n 15 cargo test -p alice-memory --test cli kill_minus_nine -j 4`. `pgrep -fl "alice daemon run"` empty afterwards. The `#[ignore]` real-model variant is the controller's to run.
- [ ] **Step 5: Commit** — `git add crates/alice-daemon/tests/import_concurrency.rs crates/alice/tests/cli.rs crates/alice-core/src/import/fixture.rs crates/alice-daemon/src/importer.rs docs/benchmarks/m3.md && git commit -m "test: concurrency and crash safety while importing"`

---

### Task 17: Gold corpus — generator and the frozen synthetic corpus

**Why:** M3 acceptance: "gold-set search quality beats MemPalace" (spec §12) with the §10 method — nDCG@10 and Recall@10 on a synthetic multilingual fixture that lives in the repo. The corpus must exist, reviewed and frozen, **before** either system runs on it.

**Files:**
- Create: `scripts/gen_gold_corpus.py` (deterministic, `random.Random(20261014)`), `crates/alice-daemon/tests/data/gold_corpus.json`
- Test: a Rust consistency test in `crates/alice-daemon/tests/import_quality.rs` (created here, extended in Task 18)

**Corpus format (also what the harness of Task 18 reads — a *gold file* is the same shape without `drawers`):**

```json
{ "drawers": [ { "id": "g001", "text": "…", "meta": { "wing": "pg_ai", "room": "decisions", "added_by": "claude", "source_file": null, … } } ],
  "queries": [ { "id": "q01", "q": "…", "category": "fact", "lexical_ok": true, "relevant": { "g001": 2, "g014": 1 } } ] }
```
`relevant` maps drawer id → grade (0/1/2; the synthetic corpus uses 1 and 2).

**Corpus (synthetic, no real data):** ~160 drawers and 48 queries. Drawers: 40 English knowledge notes (decisions, bugfix, howto), 25 Thai knowledge notes, 15 mixed Thai/English, 20 identifier/path notes (`HostLinkReconnect`, `writer_lease`, `internal/teacher/jev/client.go`), 35 transcript chunks in 6 files across 3 projects (one wing spelled `wing_pg_ai`), 20 code-file chunks, 10 exact/near duplicates, 5 diary entries. Queries: 14 `fact` (English), 8 `thai`, 6 `mixed`, 8 `identifier`, 6 `paraphrase` (no shared content words with the answer), 3 `cross_lingual` (Thai question, English answer), 3 `raw_only` (the answer exists only in a transcript chunk). `lexical_ok` is true when a plain word match suffices. **The corpus and the query list are committed first, in their own commit, before either system is run on them.** Nobody edits the corpus after seeing a result (that would be tuning to win); a corpus bug found later is fixed in a separate commit that names the reason, and both systems are re-run.

- [ ] **Step 1: Write the failing consistency test**

```rust
#[test]
fn the_gold_corpus_is_consistent() {
    let c: serde_json::Value = serde_json::from_str(include_str!("data/gold_corpus.json")).unwrap();
    let ids: std::collections::HashSet<_> = c["drawers"].as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), c["drawers"].as_array().unwrap().len(), "unique drawer ids");
    assert_eq!(c["queries"].as_array().unwrap().len(), 48);
    for q in c["queries"].as_array().unwrap() {
        assert!(!q["relevant"].as_object().unwrap().is_empty(), "{} has no relevant drawer", q["id"]);
        for (d, g) in q["relevant"].as_object().unwrap() { assert!(ids.contains(d.as_str()), "{d}"); assert!(matches!(g.as_u64(), Some(1 | 2))); }
    }
    let cats: std::collections::BTreeMap<_, usize> = c["queries"].as_array().unwrap().iter().fold(Default::default(), |mut m, q| { *m.entry(q["category"].as_str().unwrap().to_string()).or_default() += 1; m });
    assert_eq!(cats["fact"], 14); assert_eq!(cats["thai"], 8); assert_eq!(cats["mixed"], 6); assert_eq!(cats["identifier"], 8);
    assert_eq!(cats["paraphrase"], 6); assert_eq!(cats["cross_lingual"], 3); assert_eq!(cats["raw_only"], 3);
}
```
- [ ] **Step 2: Run to verify it fails** — `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --test import_quality corpus -j 4`. Expected: FAIL (no file).
- [ ] **Step 3: Write `gen_gold_corpus.py`, run it, review the JSON by eye** (languages, unique ids, every relevant id exists, Thai text is natural enough to be a fair test, no real data or names).
- [ ] **Step 4: Run the test** — PASS.
- [ ] **Step 5: Commit the corpus alone** — `git add scripts/gen_gold_corpus.py crates/alice-daemon/tests/data/gold_corpus.json crates/alice-daemon/tests/import_quality.rs && git commit -m "test: synthetic gold corpus for the MemPalace comparison"`

---

### Task 18: Gold harness, MemPalace baseline, Alice-vs-MemPalace run

**Why:** the comparison itself. The harness reads queries and relevance labels **from a file**, so the synthetic corpus today and any other gold file later use the same code.

**Prerequisites:** Task 17 committed; Tasks 13–14 done; M2 Tasks 13 (floor policy) and 16 merged (the real-model run is on the M2 head that decides the cross-lingual/paraphrase categories).

**Files:**
- Create: `scripts/m3_mempalace_baseline.py`, `crates/alice-daemon/tests/data/mempalace_baseline.json`
- Modify: `crates/alice-daemon/tests/import_quality.rs`, `docs/benchmarks/m3.md`
- Test: `crates/alice-daemon/tests/import_quality.rs`

**Interfaces:** metrics are `alice_core::judge::metrics::ndcg_at(grades_in_rank_order, pool, k)` / `dcg(grades, k)` with 0/1/2 grades (read their doc comments; they exist) plus a new 5-line `recall_at_k(ranked_ids, relevant_ids, k)` in the test file with a hand-computed unit test. Gold file loader: `GoldFile::load(path) -> GoldFile { queries: Vec<GoldQuery { id, q, category: Option<String>, lexical_ok: bool, relevant: BTreeMap<String, u8> }> }`; `ALICE_GOLD_FILE=<path>` overrides the synthetic corpus in the ignored run (the palace is then whatever `ALICE_GOLD_PALACE=<folder>` names — used only on copies).

**Baseline script** (`scripts/m3_mempalace_baseline.py`, local only, needs the owner's installed MemPalace at `/Users/icue/.venv`): builds a **throwaway** palace from `gold_corpus.json` in a temp dir and records MemPalace's top-10 for every query into `mempalace_baseline.json` (`{"mempalace_version": "…", "queries": {"q01": ["drawer_id", …]}}`). Safety rules the script enforces itself and the task verifies:
1. `python -I`, environment `HOME=<temp>` (config isolation), `MEMPALACE_PALACE_PATH=<temp palace>`, `MEMPALACE_HUB_FORWARD=0`, `MEMPALACE_DAEMON_STATE_ROOT=<temp>`; drive MemPalace through its **Python API** on that path (not the CLI, which routes writes through the hub): read `mempalace/backends/sqlite_exact.py`, `mempalace/searcher/` and `mcp_server/tools_write.py`/`tools_read.py` for the calls; use `sqlite_exact` so the fixture and the real palace share a backend.
2. Before and after, `shasum` `~/.mempalace/config.json` (read via the real path) and record `stat` of the real palace file (size/mtime only); at the end check with `lsof -p <os.getpid()>` that this process never held a file under the real palace folder or `~/.mempalace`; abort (exit 3) if the config hash changed or such a file was open.
3. If MemPalace's embedding model is not available offline in that setup, **stop and tell the controller** instead of downloading anything.

**Tests:**

```rust
#[tokio::test]
async fn gold_queries_with_a_plain_word_match_are_found_after_an_import() {          // CI: fake embedder, lexical only
    let gold = GoldFile::load(corpus_path());
    let f = Flow::from_gold_corpus().await;                                           // build_sqlite_exact from gold_corpus.json, import, wait
    let (hits, total) = f.recall_at_10(&gold, |q| q.lexical_ok).await;
    assert!(hits as f64 / total as f64 >= 0.90, "{hits}/{total}");
    assert!(f.score(&gold).ndcg_at_10 >= 0.80);
}
#[test]
fn recall_at_k_matches_a_hand_computed_example() { /* ranked [a,b,c,d], relevant {b,d,x}, k=3 → 1/3 */ }
#[test]
fn the_baseline_file_matches_the_corpus() { /* every baseline query id exists in the corpus; every listed drawer id exists; 48 queries; guards a stale baseline */ }
#[test]
fn a_gold_file_without_drawers_loads_too() { /* the loader accepts {"queries":[…]} only — the shape a later, private gold file has */ }

#[tokio::test]
#[ignore = "real e5 model (linked from ~/.alice/models); allowed by the controller — local, no cloud"]
async fn alice_beats_mempalace_on_the_gold_set() {
    let gold = GoldFile::load(std::env::var("ALICE_GOLD_FILE").map(PathBuf::from).unwrap_or_else(|_| corpus_path()));
    let f = Flow::from_gold_corpus_real_model().await;                                // waits for background embedding to finish
    let alice = f.score_all(&gold);                                                   // ndcg@10 and recall@10, overall and per category
    let base = MemPalaceBaseline::load();                                             // same metric code on the recorded lists
    eprintln!("{}", comparison_table(&alice, &base));                                 // markdown, pasted into docs/benchmarks/m3.md
    assert!(alice.overall.ndcg_at_10 > base.overall.ndcg_at_10, "{} vs {}", alice.overall.ndcg_at_10, base.overall.ndcg_at_10);
    assert!(alice.overall.recall_at_10 >= base.overall.recall_at_10);
    for c in CATEGORIES { assert!(alice.by_cat[c].ndcg_at_10 + 0.05 >= base.by_cat[c].ndcg_at_10, "{c} regressed"); }
}
```
A hit is mapped to its drawer ids through `legacy_ids` (reverse lookup: all legacy ids pointing at the hit's memory or chunk), so the survivor of a collapsed duplicate counts for every original id.

- [ ] **Step 1:** write the tests above (CI test and baseline-consistency test first); run — Expected: FAIL.
- [ ] **Step 2:** write `m3_mempalace_baseline.py`; run it **once** under the safety rules; commit the script and `mempalace_baseline.json`. If it cannot run safely, stop and tell the controller; do not fake a baseline.
- [ ] **Step 3:** implement the helpers; `env -u TYPESAFE_API_KEY nice -n 15 cargo test -p alice-daemon --test import_quality -j 4` — PASS for the CI tests.
- [ ] **Step 4:** the real-model run is **allowed by the controller** (local, no cloud, 160 synthetic drawers): link the model as in the scratch prelude and run, one heavy run, alone: `… -- --ignored --nocapture alice_beats_mempalace_on_the_gold_set`. Paste the table into `docs/benchmarks/m3.md`. **If Alice does not beat MemPalace, report the numbers as they are and stop: that is a finding for the controller, not something to fix by editing the corpus.**
- [ ] **Step 5: Commit** — `git add scripts/m3_mempalace_baseline.py crates/alice-daemon/tests/data/mempalace_baseline.json crates/alice-daemon/tests/import_quality.rs docs/benchmarks/m3.md && git commit -m "test: gold-set quality comparison against recorded MemPalace results"`

---

### Task 19: Acceptance on a snapshot of the owner's palace

**Why:** M3 is accepted when "the owner's palace imports with zero errors; gold-set search quality beats MemPalace; existing `mempalace_*` prompts work against Alice" (spec §12). The first and third are checked here on real data **copied** by the one audited snapshot path, with a throwaway home and nothing leaving the machine; the second on the synthetic set (Task 18) plus the owner's real past searches (step 6). This task produces **evidence** (numbers committed in `docs/benchmarks/m3.md`); the prose is Task 20.

**Prerequisites:** Tasks 12, 15, 18 done; M2 Task 18 merged (docs/README collisions resolved); the release binary is built **once** and reused by every step (and by Task 16's release run): `source "$HOME/.cargo/env"; nice -n 15 cargo build --release -p alice-memory -j 4`.

**Environment for every command in this task:** `export S=…scratchpad ALICE_HOME=$S/m3-home ALICE_JUDGE_PROVIDER=heuristic; unset TYPESAFE_API_KEY`, and `env -u TYPESAFE_API_KEY` on each invocation. Private material (mined queries, labels, texts) lives only under `$S/private/` and is never committed; only aggregate numbers go into the repo.

- [ ] **Step 1: Snapshot the real palace through the audited path.** Record the palace folder first, then run the one command that touches it, then record again:

```bash
P=/Users/icue/Code/AI/memory_vault
( cd "$P" && ls -la && shasum -a 256 sqlite_exact.sqlite3 knowledge_graph.sqlite3 && xxd -l 20 sqlite_exact.sqlite3 ) > "$S/palace-before.txt"; ls -la "$(dirname "$P")"/tunnels.json 2>/dev/null >> "$S/palace-before.txt"
rm -rf "$S/pal"; mkdir -p "$S/pal"
env -u TYPESAFE_API_KEY target/release/alice import snapshot --palace "$P" --out "$S/pal" | tee "$S/snapshot-notes.txt"
( cd "$P" && ls -la && shasum -a 256 sqlite_exact.sqlite3 knowledge_graph.sqlite3 ) > "$S/palace-after.txt"
diff "$S/palace-before.txt" "$S/palace-after.txt"; pgrep -fl "mempalace" | head
```
  Header bytes 18–19 = `02 02` mean WAL. Expected: the only differences are new empty `-shm`/`-wal` entries (if they were absent), or none; the main file's SHA-256 is unchanged **unless the owner's own MemPalace processes wrote in between** (`pgrep` shows them; then the size/mtime of the main file and the `-wal` timestamps explain it — say so in the report, do not hide a difference). The snapshot reported any restart note (A9). The copy at `$S/pal/` holds `sqlite_exact.sqlite3`, `knowledge_graph.sqlite3` and `tunnels.json` when the palace has them. From here on **only `$S/pal` is ever named**.
- [ ] **Step 2: Dry run, real import, re-run** (fake embedder: no CPU goes to 119k embeddings yet):

```bash
export ALICE_EMBEDDER=fake; rm -rf "$ALICE_HOME"; mkdir -p "$ALICE_HOME"; A=target/release/alice
env -u TYPESAFE_API_KEY $A import mempalace --dry-run --palace "$S/pal" | tee "$S/dry-run.txt"
env -u TYPESAFE_API_KEY $A import mempalace --palace "$S/pal" --json > "$S/import-report.json"; echo "exit $?"
env -u TYPESAFE_API_KEY $A import mempalace --palace "$S/pal" | tee "$S/rerun.txt"
```
  Expected: exit 0; `errors` empty; `drawers_seen == drawers_accounted`; the re-run imports 0 with `already_imported` = the first run's accounted total. Inspect counts only (never drawer text): `jq '{tiers, duplicates, skipped, split, review_items, facts, tunnels, blocked_by_redaction, closets_dropped, isolated_entities, phases, peak_rss_mb}' "$S/import-report.json"`, the wing map's `inferred`/`global` shares. If more than 5% of knowledge drawers land in `global`, or a wing/transcript folder maps surprisingly, **stop and show the controller the map** (a decision for the owner: `--map-wing`).
- [ ] **Step 3: Verify the data** (daemon stopped first: `$A daemon stop`; read-only SQL on our throwaway db): counts per tier and project vs the report; `SELECT count(*) FROM memories WHERE judge_state <> 'skipped' OR status <> 'active'` = 0; `PRAGMA quick_check` = ok; secret scan through FTS, not `LIKE` over 300 MB: `sqlite3 "$ALICE_HOME/alice.db" "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'ghp OR xoxb OR AKIA'"` style queries for the detector prefixes the redactor knows, expected 0 for unredacted forms (markers `[REDACTED:…]` are expected). Compat on real shapes: `$A config get compat.mempalace` → true (auto-enabled); via the HTTP route or MCP, call `mempalace_status`, `mempalace_list_wings`, `mempalace_search {"query": "<a non-sensitive phrase>", "wing": "pg_ai"}`, and `mempalace_get_drawer` on a diary id taken from the **copy**: `sqlite3 "$S/pal/sqlite_exact.sqlite3" "select id from documents where collection_id=1 and id like 'diary_%' limit 1"`; print only keys and counts.
- [ ] **Step 4: Performance on real volume** (fake embedder, lexical + chunk path): `$A daemon start`; 200 `recall --raw` calls drawn from 40 common and 40 rare words; record p50/p95, daemon RSS at idle and after the recalls (`ps -o rss= -p <pid>`), import wall time, records/second and `peak_rss_mb` from the report. Put the numbers and the machine in `docs/benchmarks/m3.md`. `$A daemon stop`.
- [ ] **Step 5: Knowledge-tier embedding with the real model (bounded).** Second throwaway home `$S/m3-home-real` (model linked from `~/.alice/models`, `ALICE_EMBEDDER` unset), same import. `pending()` embeds memories before chunks: wait until the knowledge tier is fully embedded (`embed_backlog` for memories 0; poll `alice status`), let the worker run on until about 5,000 chunks are embedded, then `alice daemon stop`. The whole step runs under `timeout 45m nice -n 15`. The raw tier then has about 5k embedded chunks (the earliest imported, so a biased sample — say so) and everything else is lexical-only; every quality number below carries that caveat.
- [ ] **Step 6: Real-query quality (owner's decision).** In `$S/private/` (never in git):
  1. **Mine** (`mine_queries.py`, written in scratch): read-only walk of `~/.claude/projects/**/*.jsonl`; collect the `query` argument of `mempalace_search` tool calls (MCP tool names ending `mempalace_search`) and the quoted query of `mempalace search "…"` shell calls; normalise whitespace; deduplicate; drop trivial ones (under 3 words, under 12 characters, obvious test strings); keep 40–80 (deterministic sample, seed 20261014). Print counts only.
  2. **Pool:** for each query take Alice's top 10 (`recall --raw --json` against the real-model home) and MemPalace's top 10 from **MemPalace's own search code run read-only against `$S/pal`** (Python API, `python -I`, `MEMPALACE_PALACE_PATH=$S/pal`, `MEMPALACE_HUB_FORWARD=0`, the same safety guard as Task 18's baseline script: config hash, no open file under the real palace or `~/.mempalace`). Map every hit to a drawer id; for text use **Alice's stored (redacted) text** looked up by legacy id, never MemPalace's raw text; a hit without an Alice row is shown as "(not imported: <reason>)" to the grader.
  3. **Blind grading:** shuffle each query's pooled results, strip system names and ids, give each a random label, truncate texts to 1,200 characters, and have **one Claude subagent** (`model: "sonnet"`, via the Agent tool; briefed with the query and the shuffled candidates only, never told which system produced which) return a JSON grade 0/1/2 per candidate (0 irrelevant, 1 partly useful, 2 answers it). This is the owner's explicit decision (the data stays between this machine and Claude); no Jev, no other service.
  4. **Score:** nDCG@10 and recall@10 (pooled relevant = grade ≥ 1) for both systems; **paired bootstrap** over queries (10,000 resamples, seed 20261014) for the difference with a 95% interval. Commit only the aggregate table (counts of queries, both systems' means, the interval, the caveats of step 5) to `docs/benchmarks/m3.md`; the queries, texts and labels stay in `$S/private/`.
  If the interval is negative or spans zero, report it exactly so; do not tune.
- [ ] **Step 7: Commit the evidence** — `git add docs/benchmarks/m3.md && git commit -m "docs: M3 acceptance evidence on a snapshot of the owner's palace"` (numbers only).

---

### Task 20: Docs, spec as built, handoff, push

**Files:**
- Modify: `README.md` and `README.th.md` (a short "Importing from MemPalace" and "MemPalace compatibility" section, same commit), `docs/superpowers/specs/2026-10-13-alice-core-design.md` (§8.2 and §9 as built; the Plan-decision deviations), `docs/benchmarks/m3.md` (prose around the numbers)
- Create: `docs/superpowers/handoff/2026-10-14-m3-handoff.md`

- [ ] **Step 1: README (both languages):** the command, `--dry-run`, safe to re-run, "reads a read-only snapshot; MemPalace need not be stopped", what maps where in five lines, `compat.mempalace`, how to register `alice mcp` under the MCP server name old prompts use (the owner's prompts call `mcp__plugin_mempalace_mempalace__mempalace_*`, so the server entry must carry that name), and the `--allow-remote-embedding` caution.
- [ ] **Step 2: Spec as built:** §9 gets an "As built" note listing each deviation (Plan decisions 2, 3, 7, 8, 15, 19, 21, 22 below); §8.2 notes that unsupported tools are answered but not listed and that compat is gated by the shared switches.
- [ ] **Step 3: Handoff:** where things stand, ledger path, commands to resume, every ruling made, open items (the M2 coordination list below, the real-palace caveats of Task 19 step 5, anything reported "not run").
- [ ] **Step 4: Final checks and push.** `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `env -u TYPESAFE_API_KEY nice -n 15 cargo test --workspace -j 4` (one heavy run), `pgrep -fl "alice daemon run"` empty, `git status` clean except intended files, `git merge --no-edit feat/m2-judge-settings` if it moved and re-run the affected tests. Then:

```bash
git add README.md README.th.md docs/benchmarks/m3.md docs/superpowers/specs/2026-10-13-alice-core-design.md docs/superpowers/handoff/2026-10-14-m3-handoff.md
git commit -m "docs: M3 README, spec as built and handoff"
git push -u origin feat/m3-import
```

**M2 coordination list (write into the ledger; tell the controller at the end of Phase 2):** (1) migration number of `legacy_ids` (0004 expected); (2) the `rusqlite` `backup` feature line shared with M2 Task 14; (3) `judge_state = 'skipped'` + `status = 'active'` is what M3 writes for imported memories, M2's pipeline only picks `'pending'`/`'retry'` (confirmed by the controller); (4) compat MCP gating generalises M2 Task 11's admin switch into `Switches` (additive; its tests stay green); (5) `store::projects::upsert_project` gained placeholder adoption (new behaviour only for `import:` keys); (6) the knowledge graph of the owner's palace lives in the palace folder — `~/.mempalace/knowledge_graph.sqlite3` on this machine is empty; (7) `history::undoable` lists the two import actions.

---

## Plan decisions

Each entry: what the plan chose where the spec is silent or the real data forced a choice, and the cost if wrong. The spec wins everywhere else.

1. **The daemon runs the import; the CLI only starts it and polls.** Parsing, redaction and batching are worker-thread work (spec §2.2 rules 1–3), the daemon is the only process that opens `alice.db`, and a job survives a closed terminal. *Cost if wrong:* `alice import mempalace` starts the daemon if it is not running (as every other command does) and needs it reachable.
2. **Knowledge widening.** Spec §9: knowledge = diary drawers + drawers written by agents (`added_by` set, no source file). The owner's palace has 34,600 file-mined code drawers whose `added_by` is an agent name, so `added_by` alone would make them knowledge. The plan keeps the spec rule and adds one case: a drawer **with** a `source_file` in a curated room (`decisions`, `knowledge`, `status`, `progress`, `runbook`, `methods`, `bug-classes`, `issues`) written by an agent is knowledge too (31 drawers). *Cost if wrong:* up to ~31 notes sit in the raw tier (still searchable with `include_raw`/fallback).
3. **A `legacy_ids` table, not an index on `chunks.legacy_id`.** One lookup structure for compat resolution (including drawers collapsed into a survivor), the "already imported" check, and KG triples. *Cost if wrong:* ~120k extra small rows and a migration whose number must be reconciled with M2's.
4. **Imported projects are placeholders (`import:<slug>`) that the first real project of the same slug — or of a slug equal to one of the placeholder's aliases — adopts.** Otherwise the first `remember` from the real checkout creates `pg-ai-1a2b3c` beside the imported `pg-ai`. A real checkout named `project-alice` does not adopt an imported `alice` unless that alias exists (`--map-wing` can create it). *Cost if wrong:* two unrelated repositories that both slugify to `api` share the imported project (the imported wing was ambiguous anyway).
5. **Imported memories are written `status = 'active'`, `judge_state = 'skipped'`; the import itself calls no Judge and sends nothing off the machine.** M2's judge picks only `'pending'`/`'retry'`, so imports are not judged themselves; once imported they are ordinary memories governed by the same cloud gate (`SettingsHandle::cloud_allowed`) — a later judged `remember` may include them as neighbours, and an HTTP embedding provider would receive every chunk, which is why `start_import` refuses a non-local `embedding.provider` without `--allow-remote-embedding` (controller ruling, A20). Every real-data command also runs with `ALICE_JUDGE_PROVIDER=heuristic` and without the Jev key. *Cost if wrong:* imported knowledge is not related/superseded automatically; a later "judge backlog" command can do it deliberately.
6. **MemPalace's naive timestamps are read in the machine's UTC offset at import time** (MemPalace writes local naive times); the offset is printed in the report. *Cost if wrong:* times shift by the difference if the palace was built in another zone.
7. **The per-batch checkpoint is the `legacy_ids` rows written in the batch's own transaction**; the `sync_state` row is informational. No resume-from-cursor shortcut: drawer ids are hashes, so a drawer added since the last run can sort anywhere, and a cursor would skip it. *Cost if wrong:* a re-run re-plans every drawer (reads 1 GB again, seconds) before skipping what is stored.
8. **Collapse rules.** Raw: same source and identical text → one chunk (earliest `(chunk_index, filed_at, id)` kept). Knowledge: Alice's own `content_hash` (project, kind, title, body) at insert. Identical raw text in *different* files is kept (two files may legitimately hold the same lines). *Cost if wrong:* some repeated boilerplate chunks remain in the raw tier, which ranks at 0.6.
9. **Wing → project is data-driven** (a `wing_x` is an alias when a plain wing `x` exists, otherwise an agent wing whose project is inferred from the text) with a `--map-wing wing=project` override for names the data cannot decide (`wing_project_alice` → `alice`). *Cost if wrong:* a few agent-style wings land in `global` until the owner adds an override and re-imports (re-import only fills what moved; existing rows are not re-homed — see the handoff).
10. **Compat writes to an unknown wing go to the caller's current project** (with a `note`), never create a project (spec §3.2: agents never create containers). *Cost if wrong:* an old prompt that invents wings files into the current project instead of a new one.
11. **Compat `similarity` is the hit's score relative to the best hit** (1.0 for the best); Alice's fused scores are not cosines and must not be presented as such. *Cost if wrong:* a prompt that thresholds on MemPalace distances behaves differently.
12. **Unsupported `mempalace_*` tools are answered (not errors) but not listed.** Spec §8.2 says they are "not provided; calling them returns a message". *Cost if wrong:* none beyond a different answer shape.
13. **Compat gating is evaluated on every call** (no restart); the `tools/list_changed` notification is a nice-to-have. *Cost if wrong:* a client that cached the list sees the change at its next list.
14. **The gold comparison freezes the corpus before either system runs and records MemPalace's answers once; the real-model run on the 160 synthetic drawers is allowed (local, no cloud).** Queries and labels are read from a file, so other gold files plug in. The real-palace quality check (Task 19 step 6) uses the owner's own past searches mined read-only from local Claude Code transcripts, blind-graded by one Claude subagent over Alice's redacted text, with a paired bootstrap; private material stays in the scratch directory and only aggregates are committed. *Cost if wrong:* a stale baseline (guarded by `the_baseline_file_matches_the_corpus`); a grader's bias is bounded by blinding and shuffling.
15. **The knowledge graph is read from the palace folder first, then `~/.mempalace/`.** On this machine the folder's file has the 159 triples and `~/.mempalace/knowledge_graph.sqlite3` is empty; the lane brief names the latter. *Cost if wrong:* an owner with only the home file is served by the fallback.
16. **The snapshot lives under `ALICE_HOME/import-tmp/<job>/`** (private, same volume, removed by a drop guard and swept at daemon start; excluded from M2 backups) and needs about the palace's size in free space (~1 GB for the owner's); a clear message says so when the volume is full. *Cost if wrong:* a machine short on disk must free space or import from a smaller copy.
17. **Embeddings are recomputed in the background; MemPalace's MiniLM vectors are ignored.** Spec §9. *Cost if wrong:* none.
18. **Only explicit tunnels that name two importable drawers become links.** Passive "tunnels" (shared room names) are not links. *Cost if wrong:* some cross-project relatedness is not carried over.
19. **Accounting law.** The report carries `drawers_seen` and `drawers_accounted` (imported memories + imported chunks + collapsed duplicates + skipped + blocked + already imported); a mismatch is an `errors` entry and a non-zero exit. This turns "zero errors" into a checkable statement. *Cost if wrong:* a legitimate new category not added to the sum shows up as a false error (fixed by naming it).
20. **One audited snapshot path.** `import::palace::snapshot()` is the only code that opens the owner's files (read-only, backup API with a restart limit and a time-box, WAL-aware); the acceptance copy is made by the hidden `alice import snapshot`, never by Python or `cp`. *Cost if wrong:* a second hidden subcommand to maintain.
21. **Entities without a fact are not imported** (266 entities vs 159 triples on the owner's palace; entity `type` was `unknown` for all). The report counts them. *Cost if wrong:* a named-but-unconnected entity is missing from the graph until an agent mentions it.
22. **Redaction runs before splitting, on every string that reaches the writer, with the always-on redactor,** whatever `privacy.redact` says (the report says so when it is off). *Cost if wrong:* a deliberately raw-storing user gets redacted history from an import (they can re-import never; the setting is respected for everything else).
23. **Tasks were split for review size and renumbered** (old → new: 5→5+6, 8→9+10+11, 10→13+14, 13→17+18, 14→19+20). *Cost if wrong:* none beyond cross-references.
24. **The shim reads compat from the same cached configuration switches as `mcp.admin_tools`** (no daemon round trip, 1-second TTL), so `alice mcp` start-up stays under 20 ms. *Cost if wrong:* a client sees a compat change up to a second late.

## Self-review notes (for the executor)

- **Spec §9 coverage:** read directly + `sqlite_exact` and `chroma` + KG (Tasks 2, 3, 7); snapshot with the backup API, WAL-safe and bounded (2); mapping: transcript → raw session sources/ordered chunks, file chunks → raw file, diary and agent drawers → knowledge with kinds from rooms, wings → projects through normalisation and aliases and agent wings inferred then `global`, rooms → topics, closets dropped and counted, exact duplicates collapsed with the original ids kept in `legacy_ids`, KG with validity and predicate review items, explicit tunnels → `relates_to` links (4, 5, 6, 7, 8); AAAK text kept verbatim (5: text is never rewritten beyond NUL/NFC/redaction); embeddings in the background (9–11, decision 17); resumable and incremental (`--since`, `legacy_ids`, drawer-atomic batches, stable ords) (1, 6, 8, 10, 16); final report with accounting law (9, 11). **§8.2:** the 15 mapped tools (13, 14), unsupported tools answered with the Alice equivalent (13), opt-in setting enabled after import and live shared gate (11, 15). **§12 M3 acceptance:** zero errors on the owner's palace (19 steps 2–3), gold-set quality (17, 18, 19 step 6), `mempalace_*` prompts work (13–15, 19 step 3). **§10:** unit tests per module (all), concurrency regression with an import (16), crash safety during import (16), gold sets (17, 18), interface tests (15), security corpus on imported text (5, 7, 10, 11).
- **Names used across tasks** (check on every task): `DrawerMeta` (incl. `unreadable`), `Drawer`, `PalaceReader::{info,page,get,wing_counts}`, `Snapshot {dir, backend, source, kg, tunnels, notes}`, `snapshot(palace, dest, mempalace_home)`, `open_snapshot(&snap, collection)`, `WingMap::{build,target,projects,aliases,decide,project_for_transcript,note_transcript_dir}`, `ProjectDecision`, `Via`, `Tier`, `TextFilter`/`NoFilter`, `Mapper::map`, `Survey`, `survey(.., reserved)`, `Planner::plan`, `Planned::{Memories,Chunks,Skipped,Blocked,Collapsed}`, `PlannedMemory`, `PlannedChunk`, `KgPlan`, `PlannedFact`, `Tunnels`, `ImportBatch`, `ImportMemory`, `ImportSource`, `ImportFact`, `ImportProject`, `BatchOutcome`, `apply_batch`, `legacy_put/get/existing`, `ImportRequest`, `ImportStatus`, `ImportState`, `ImportReport`, `ImportSlot`, `CompatCall`, `CompatResult`, `SUPPORTED`, `CompatDrawer`, `HitDetail`, `Switches`, `GoldFile`.
- **Prerequisite map:** Task 1 needs M2 Task 12; Tasks 8–11 need M2 Tasks 12/13/14; Tasks 13–16 need Task 11 (and M2 Task 16 for 16); Tasks 17–20 need M2 Tasks 13/16/18. If a task's prerequisite has not landed, work on another unblocked task or stop and tell the controller.
