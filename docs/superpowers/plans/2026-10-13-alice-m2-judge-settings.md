# Alice M2 — Judge, Maintenance, Settings — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship Alice milestone M2: the daemon meets its 800 MB memory budget at 1M chunks with the real local model, builds and tests on Windows, judges every new memory against its neighbours (heuristic offline, TypeSafe Jev optional) with a review inbox, events and undo, redacts secrets before anything is stored or sent, runs maintenance jobs (backups, checks, retention, purges) when idle, restores verified backups, re-embeds on a model change, and exposes every setting through a schema-backed HTTP API, `alice config` and live reload.

**Architecture:** M1's shape stays: one daemon owns `alice.db`, one writer thread applies every write in short transactions, worker threads do the heavy work and hand small batches to the writer. M2 adds three background threads (judge, maintenance, re-embed), each feeding the writer through new `WriteOp` variants; a `SettingsHandle` (layered TOML + env, polled for edits) that every component reads per use; pure decision logic in `alice-core` (`judge`, `redact`, `settings`, `bq`, `store::{events,history,reviews,judge}`) and the I/O around it in `alice-daemon` (`judge_http`, `judge_worker`, `maintenance`, `backup`, `restore`, `reembed`, `config`, `secrets`, `service_install`). The chunk vectors move from an int8 HNSW to a flat sign-bit index rescored from the f16 vectors in SQLite, and the local model runs through ONNX Runtime directly with a compact Unigram tokenizer instead of fastembed.

**Tech Stack:** Rust 1.99 (edition 2024), rusqlite 0.40 (bundled + `backup`), usearch 2.26.4 (default features off), ort =2.0.0-rc.13 (ONNX Runtime, static), spm_precompiled 0.1.4, reqwest 0.13, axum 0.8 (+ SSE), tokio 1.53, rmcp 3.5.1, schemars 1.2, toml 1.1.8, toml_edit 0.25.17, strsim 0.11.1, regex 1.13, keyring 4.2.0 (optional), clap 4.6.

**Spec:** `docs/superpowers/specs/2026-10-13-alice-core-design.md` — §12 row M2 and what it depends on: §4.1 (daily index check), §4.2 (model change, HTTP providers), §4.4 step 4 (precise mode), §4.6 (budgets), §5.1–§5.7, §6, §8.1 (admin toolset), §8.3, §8.4, §10, §11 (`alice service install`). Executors read the spec next to this plan; where they disagree the spec wins unless a "Plan decisions" entry says otherwise.

**Base:** branch `feat/m2-judge-settings`, created by the controller from the head of `feat/m1-core` after the M1 final-review fix wave (that wave was still committing while this plan was written; it moved query embedding in `Service::recall` ahead of taking a reader and changed `search/lexical.rs`). Names in this plan are those of `feat/m1-core` at 6dda1a3 plus that wave. **First step of every task:** read every file the task modifies, as it is now; when a signature named here has changed, keep the behaviour this plan asks for and adapt the call. Never "restore" code the fix wave changed.

## Global Constraints

- Toolchain: Rust stable 1.99, edition 2024, `resolver = "3"`, pinned in `rust-toolchain.toml`. Crates `alice-core`, `alice-embed`, `alice-daemon`, binary package `alice-memory` (`[[bin]] name = "alice"`), all `license = "MIT"`, `repository = "https://github.com/icueth/alice"`.
- Data directory: `ALICE_HOME`, default `~/.alice` on every OS; layout per spec §2.3 (M2 adds `secrets.toml`, `vectors/<model>.chunks.bq`, `backups/alice-*.db`).
- IDs: ULID (26 uppercase chars). Timestamps: RFC 3339 UTC with milliseconds and `Z`, stored as TEXT. HLC: `{millis:013}-{counter:04x}-{device}`.
- Performance budgets (spec §4.6), never relaxed: `remember` ack p95 < 50 ms; `recall` fast p95 < 200 ms at 1M chunks; `context` p95 < 150 ms; `alice mcp` start-up < 20 ms; **daemon RSS < 800 MB at 1M chunks with the real local model**, measured as the daemon's peak RSS (and, on macOS, peak physical footprint) over a bench run that loads the model, embeds new memories and serves 4 bursts of 64 concurrent recalls.
- Recall quality never drops: `crates/alice-core/tests/search_quality.rs` and the ignored real-model `crates/alice-embed/tests/real_search_quality.rs` keep passing with unchanged thresholds.
- Only the daemon opens `alice.db`; clients never touch it. The one exception is `alice restore`, which swaps files while it holds the daemon lock (no daemon can run), and never opens `alice.db` itself.
- All writes go through the single writer thread (`WriteOp`). Workers (embedding, judge, maintenance, re-embed) read through the reader pool or a dedicated read-only connection and hand small batches to the writer. Writer transactions stay short: ingest slices ≤ 64 KB / 200 chunks; maintenance deletes ≤ 500 rows per `WriteOp`; FTS merges ≤ 500 pages per `WriteOp`.
- A Judge failure never fails a write; network calls never run on the writer thread or an async runtime thread (use `spawn_blocking` or a worker thread).
- Limits from M1 stand (content ≤ 65,536 bytes, title ≤ 200 chars, ≤ 20 topics, importance 1–5, chunk ≤ 32,768 bytes, ≤ 1,000 chunks per ingest, recall limit ≤ 50).
- Server binds 127.0.0.1 only; every API/MCP request needs `Authorization: Bearer <token>`; `Host` must be `127.0.0.1:<port>` or `localhost:<port>`; any `Origin` must equal the daemon origin. New routes go behind the same guard.
- Error text returned to agents and CLI users: one or two sentences saying what happened and what to do next.
- MCP core tool names stay exactly `recall`, `remember`, `context`, `get_memory`, `update_memory`, `query_facts`, `assert_fact`. Admin tools (`review_list`, `review_resolve`, `merge_memories`, `stats`) are listed only when `mcp.admin_tools = true`.
- Jev: endpoint `https://api.typesafe.ai/v1/systemone`, model pinned `jev-1.13.0`, header `Authorization: Bearer <key>`, body `{model, state, questions:{<id>:{type:"choice", instructions, criteria:{<option>: <description>}}}}` (criteria is an object, never an array), response `answers.<id>.{choice, confidence, probabilities}` and `usage.{input_tokens, output_tokens}`; price $0.042 per million input tokens, output free; ≤ 10 requests/s; retries only on 429, 5xx and timeouts.
- Secrets: `~/.alice/secrets.toml` with mode 0600 by default; OS keychain only when `secrets.backend = "keychain"`. The API never returns a secret value, only `{set, hint}`. Secret values never appear in logs, events, fixtures, reports or commits.
- Secret-shaped test strings (provider tokens, API keys, PEM headers) never appear as one literal in any committed file: build them with `concat!` split inside the provider prefix (e.g. `concat!("gh", "p_…")`, `concat!("sk_", "live_…")`). GitHub push protection rejects a push that contains them, even fake ones.
- The TypeSafe key for live tests and the evaluation lives in the repo's git-ignored `.env` as `TYPESAFE_API_KEY`. Load it only with `set -a; . ./.env; set +a` in the shell that runs the ignored test; never `cat`, `echo`, print, log or commit it, and never write it into a fixture. Recorded Jev fixtures contain request and response bodies only, never headers.
- Unit and integration tests never touch the network; live tests are `#[ignore]` and read keys from the environment only.
- Heavy commands on the owner's machine: `source "$HOME/.cargo/env"` first; `nice -n 15 cargo build|test|clippy -j 6`; one heavy run (build, test suite, bench, eval) at a time; never run benchmark variants in parallel.
- Scratch directory for binaries, bench data and manual runs: `S=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2` (absolute, outside the repo). Bench data in `$S/data-*` is reused across tasks and deleted in Task 18.
- **Every manual or shell step in every task** (cargo runs, bench runs, daemons started by hand, `alice …` calls) runs with `ALICE_HOME` exported to a throwaway directory under `$S` for the whole step, never the real `~/.alice`. Each such step starts with the **scratch prelude**:
  `export S=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2 ALICE_HOME=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2/home; mkdir -p "$ALICE_HOME"; [ -e "$ALICE_HOME/models" ] || ln -s "$HOME/.alice/models" "$ALICE_HOME/models"`
  (the link only lets `--real` runs read the downloaded model; tests set their own homes, so the export is harmless to them and catches one that forgets). `alice bench --dir $S/data-…` keeps this home (bench refuses a `--dir` equal to the home in use). A step that runs other `alice` commands or a daemon against bench data exports `ALICE_HOME=$S/data-…` for every command of that step (and links the model into it the same way when it needs the real model). `alice daemon stop` at the end of such a step stops that home's daemon only.
- Never edit the user's agent configs (`~/.claude*`, `~/.codex`) or the real `~/.alice` beyond reading `~/.alice/models`. Tests use temporary `ALICE_HOME`s. `alice service install` is tested only with `--dry-run` or a temporary `--target-dir`; tests never call `launchctl`, `systemctl` or `schtasks` for real.
- Process tests leave no stray daemons: after every test run, `pgrep -fl "alice daemon run"` prints nothing that the run started.
- The workspace builds and its tests pass after **every** task, ignored tests included (`cargo test --workspace` compiles `#[ignore]` tests even though it does not run them): a task that changes an API updates every caller, test files and benches too.
- `cargo test --workspace` passes on macOS, Linux and Windows CI (`fmt --check`, `clippy --all-targets -D warnings`, `test`). A test that cannot run on Windows is `#[cfg(unix)]` (or `#[cfg_attr(windows, ignore = "<reason>")]`) with the reason in a comment.
- Docs are English; `README.th.md` is updated in the same commit as `README.md` whenever README content changes.
- No release, no publish, no tag. Push only the feature branch (`git push -u origin feat/m2-judge-settings`).
- New dependencies are only those named in this plan, at the versions named. `Cargo.lock` is committed with every change to it.
- Before every commit: the task's tests, `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`. Commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`; the repo's git identity is preconfigured.
- A shell hook routes commands through `rtk`; when output looks filtered, stale or truncated, prefix `rtk proxy` (e.g. `rtk proxy git diff`); `rtk` drops `git -C`, so `cd` into the repo instead. Read files with the Read tool.

## Review Focus

1. **Secret-shaped text on every path to disk or network** (a GitHub token in a `remember`, an `sk-ant-` key in an edit, a PEM block in an ingested chunk, a password in a URL in a memory written while the daemon is down, a memory sent to Jev, a write the daemon logs): no raw secret reaches `alice.db`, a spool file, `daemon.log` or a Jev request body; `block` mode refuses the write naming the detector; text already redacted is left as it is. Tests owned by Task 7 (the Jev request body: Task 9).
2. **Config edits that are invalid or half-written** (an editor saving `config.toml` mid-write, a wrong type, an unknown key, `review_threshold` above `auto_apply_threshold`, an `ALICE_*` variable with a bad value, a PATCH that sets a read-only key): the previous settings stay in effect, the daemon keeps running, and the error is shown by `alice config list`, `/api/v1/settings` and `alice doctor`. An `ALICE_*` variable that is not a setting (a typo) is ignored with a warning in health, `/api/v1/settings` and `alice doctor`; it never sends the other settings back to their defaults. Tests owned by Tasks 5, 6 and 17.
3. **Judge outages and budget edges** (Jev 429 storms, 5xx, timeouts, malformed JSON, a missing or rejected key, the budget running out mid-day, the UTC day changing, a cloud-denied project): writes still succeed, the heuristic decides, `judge_state = 'retry'`, no request leaves for a denied project, a spend that cannot be read counts as an exhausted budget, and the day's spend never exceeds the daily budget (each call reserves an upper bound of its cost first). Tests owned by Tasks 9 and 12.
4. **Undo and review on targets that changed since** (undo after a later edit, undo twice, undo of a purged memory, resolving an already resolved item, a judge supersede whose old memory was edited meanwhile): nothing is silently overwritten; the result is a clear error or a review item. Tests owned by Tasks 4, 11 and 12.
5. **Backups and restore at the edges** (restore while agents write, a backup from a newer schema, a truncated or non-SQLite file, a failed backup, rotation with only one good backup): writes made during a restore are spooled and applied after it, a bad backup is refused before anything is swapped, a failed backup leaves no file in `backups/`, and rotation never deletes the newest good backup. Tests owned by Tasks 14 and 15.

---

## File Structure

```
Cargo.toml                                  + toml, toml_edit, strsim, spm_precompiled, ort; usearch default-features off; rusqlite "backup"
.github/workflows/ci.yml                    Windows job runs the full test suite (Task 3)
crates/alice-core/src/
  bq.rs                                     flat sign-bit chunk index (Task 1)
  vector.rs                                 memories: usearch int8 HNSW; chunks: bq + SQLite rescoring (Tasks 1, 3)
  settings.rs                               settings schema, layers, validation, env mapping, TOML edits, units, project scope (Task 5)
  redact.rs                                 secret detectors and redaction (Task 7)
  judge/mod.rs                              Judge trait and types (Task 8)
  judge/verbal.rs                           numbers and dates in words (Task 8)
  judge/heuristic.rs                        offline provider (Task 8)
  judge/text.rs                             title/predicate normalisation, Jaro-Winkler helpers (Task 8)
  judge/jev.rs                              Jev provider: requests, cache, budget, rate, retries (Task 9)
  judge/metrics.rs                          accuracy, macro-F1, Brier, ECE, nDCG (Task 10)
  judge/pipeline.rs                         relation decisions → actions and reviews (Task 12)
  store/sql/0002_events_reviews.sql         events.group_id, review dedupe key, indexes (Task 4)
  store/sql/0003_judge.sql                  judge columns on memories (Task 12)
  store/events.rs                           Actor, event rows (Task 4)
  store/history.rs                          snapshots, undo (Task 4)
  store/reviews.rs                          review inbox (Tasks 4, 11)
  store/judge.rs                            judge_cache, cost_ledger (Task 8)
  store/maintenance.rs                      purge, retention, requeue, clusters, events prune (Task 14)
crates/alice-embed/src/
  tokenizer.rs                              compact XLM-R Unigram tokenizer (Task 2)
  lazy.rs                                   load-on-demand slot with idle unload (Task 2)
  onnx.rs                                   OnnxEmbedder: ORT session, pooling, idle unload (Task 2)
  net.rs                                    BlockingHttp: reqwest on its own runtime, usable from any thread (Task 9)
  http.rs                                   HTTP embedding providers: openai, ollama, voyage (Task 16)
crates/alice-daemon/src/
  config.rs                                 SettingsHandle: load, patch, poll, broadcast (Task 5)
  secrets.rs                                SecretStore: file (0600) and keychain (Task 5)
  judge_http.rs                             Jev transport and store, judge construction (Task 9)
  judge_worker.rs                           relation-to-existing pipeline thread (Task 12)
  maintenance.rs                            idle scheduler and jobs (Task 14)
  backup.rs                                 online backup, rotation, listing (Task 14)
  restore.rs                                backup verification and file swap (Task 15)
  reembed.rs                                model-change re-embed and switch (Task 16)
  redaction.rs                              compiled redactors per project (Task 7)
  doctor.rs                                 health checks (Task 17)
  service_install.rs                        launchd / systemd --user / schtasks plans (Task 17)
  writer.rs, worker.rs, service.rs, http.rs, mcp.rs, client.rs, daemon.rs, spool.rs, paths.rs   (modified throughout)
crates/alice/src/
  main.rs, cmd.rs                           config, review, history, undo, backup, restore, doctor, service, token (Tasks 4–17)
  bench.rs                                  --prefill-vectors (Task 1), RSS gate (Task 2)
docs/benchmarks/m2.md                       memory, Windows and maintenance measurements (Tasks 1, 2, 3, 14)
docs/benchmarks/m2-jev.md                   Jev evaluation (Task 10)
```

## Task list

| # | Task | Depends on |
|---|---|---|
| 1 | Memory budget I: real-model 1M measurement mode, flat sign-bit chunk index rescored from SQLite | — |
| 2 | Memory budget II: ONNX Runtime embedder with a compact tokenizer, idle unload; budget verified at 1M | 1 |
| 3 | Windows build: usearch without NumKong, Unicode-safe index files, CI green on windows-latest | 1 |
| 4 | Schema v2, events with actors, history, undo, SSE stream | — |
| 5 | Settings schema, layers, validation, secrets store | — |
| 6 | Live settings: HTTP API, `alice config`, wiring, reload | 2, 4, 5 |
| 7 | Secret redaction on every write path | 6 |
| 8 | Judge core: trait, verbalisation, heuristic provider, cache and cost store | 5 |
| 9 | Jev provider: requests, cache, budget, rate limit, retries, fixtures, live smoke test | 7, 8 |
| 10 | Jev evaluation: labelled set, metrics, `docs/benchmarks/m2-jev.md` | 2, 9 |
| 11 | Review inbox: store, resolutions, CLI, HTTP, admin MCP tools, briefing line | 4, 6, 8, 9 |
| 12 | Relation-to-existing pipeline, kind and project routing, activation after judging | 9, 11 |
| 13 | Predicate normalisation, precise recall, fact time validation | 12 |
| 14 | Maintenance scheduler, backups and rotation, retention, purges | 12 |
| 15 | Restore with a verified round trip | 14 |
| 16 | HTTP embedding providers and model-change re-embed | 2, 6, 14 |
| 17 | Health, `alice doctor`, `alice token rotate`, `alice service install` | 14, 16 |
| 18 | Docs, spec as built, final verification, push | all |

---

### Task 1: Memory budget I — real-model 1M measurement mode and a flat sign-bit chunk index

The M1 daemon is projected at ~1.3 GB at 1M chunks with the local model (`docs/benchmarks/m1.md`, section Memory): the int8 HNSW over 1M chunk vectors is ~560 MB and one model instance ~600 MB. This task (a) adds a bench mode that measures the real-model daemon at 1M chunks in minutes instead of hours, and (b) replaces the chunk HNSW with a flat index of one sign bit per dimension (48 bytes per vector), whose candidates are rescored with the exact f16 vectors already stored in SQLite. Memories keep their int8 HNSW (they are few and searched on every recall). Task 2 removes the rest of the gap; this task records its share.

**Files:**
- Create: `crates/alice-core/src/bq.rs`
- Modify: `crates/alice-core/src/lib.rs` (add `pub mod bq;`)
- Modify: `crates/alice-core/src/vector.rs` (chunk part, `for_model`, `open(path, model, dim)`, `search_chunks`, legacy file removal, tests)
- Modify: `crates/alice-core/src/store/embeddings.rs` (`get_many`)
- Modify: `crates/alice-core/src/search/mod.rs` (raw vector list via `search_chunks`, one test)
- Modify: `crates/alice-daemon/src/service.rs` (`VectorIndex::open(&path, &model, dim)`, `for_model` in the rebuild fallback, the restart test's file names)
- Modify: `crates/alice-daemon/src/worker.rs` (tests: `VectorIndex::open` signature)
- Modify: `crates/alice-daemon/src/paths.rs` (doc comment of `vector_file`)
- Modify: `crates/alice/src/cmd.rs` (`BenchArgs::prefill_vectors`), `crates/alice/src/bench.rs`
- Create: `crates/alice-embed/tests/binary_chunk_recall.rs` (ignored, real model)
- Create: `docs/benchmarks/m2.md`

**Interfaces:**
- Consumes: `alice_core::embed::{Embedder, FakeEmbedder, cosine}`, `alice_core::store::embeddings::{VecKey, put, count, for_each, decode_f16}`, M1 `VectorIndex` internals (`Part`, `Gate`, `encode_key`, `decode_key`).
- Produces:
  - `alice_core::bq::{BinaryIndex, BOOTSTRAP}`: `BinaryIndex::{new(dim) -> Self, dim(&self) -> usize, len(&self) -> usize, is_empty(&self) -> bool, is_bootstrapping(&self) -> bool, upsert(&mut self, seq: i64, v: &[f32]) -> Result<()>, remove(&mut self, seq: i64) -> bool, remove_many(&mut self, seqs: &[i64]) -> usize, candidates(&self, query: &[f32], n: usize) -> Vec<(i64, f32)>, write_to(&self, w: impl Write) -> io::Result<()>, read_from(r: impl Read, dim: usize) -> Result<Self>}`
  - `alice_core::vector::{CHUNK_OVERSAMPLE: usize = 4, MIN_RESCORE: usize = 200}`
  - `VectorIndex::{for_model(model: &str, dim: usize) -> Result<Self>, model(&self) -> &str, open(path: &Path, model: &str, dim: usize) -> Result<Self>, search_chunks(&self, conn: &Connection, query: &[f32], k: usize) -> Result<Vec<(VecKey, f32)>>, search_chunks_with(&self, conn: &Connection, query: &[f32], k: usize, oversample: usize) -> Result<Vec<(VecKey, f32)>>, remove_chunks(&self, seqs: &[i64]) -> Result<usize>}`; `VectorIndex::new(dim)` = `for_model("", dim)`; `search(.., KeyFilter::Chunks | Any)` now returns *estimated* chunk similarities (ordering only).
  - Index files: `vectors/<model>.memories.usearch` and `vectors/<model>.chunks.bq`; a leftover `<model>.chunks.usearch` is deleted on the first save.
  - `alice_core::store::embeddings::get_many(conn: &Connection, model: &str, keys: &[VecKey]) -> Result<Vec<(VecKey, Vec<f32>)>>`
  - `alice bench --real --prefill-vectors`: chunk and memory vectors are written by a fake embedder that reports the real model's id and dimension; the daemon child then loads the real model, embeds 256 new memories and serves the bursts. Report fields `daemon.remembered`, `daemon.embed_new_ms`, `daemon.rss_after_embedding_mb`, `daemon.footprint_after_embedding_mb`.

- [ ] **Step 1: Read the files this task modifies, as they are now** (see the Base note at the top of the plan).

- [ ] **Step 2: Add the prefill mode to the bench**

In `crates/alice/src/cmd.rs`, add to `BenchArgs`:

```rust
    /// With --real: write the chunk and memory vectors with a fast fake embedder that reports
    /// the real model's id and dimension, so a 1M-chunk index exists in minutes; the daemon run
    /// then loads the real model, embeds new memories with it and serves the bursts. Measures
    /// the daemon's memory at scale; in-process latency figures then use fake query vectors.
    #[arg(long, requires = "real")]
    pub prefill_vectors: bool,
```

In `crates/alice/src/bench.rs`:

```rust
/// A fake embedder that answers to another model's id: vectors cost the same in the index
/// whatever their values, so a large index for the real model can be filled quickly.
struct Relabeled {
    inner: FakeEmbedder,
    id: &'static str,
}

impl Embedder for Relabeled {
    fn model_id(&self) -> &str {
        self.id
    }
    fn dim(&self) -> usize {
        self.inner.dim()
    }
    fn embed_passages(&self, texts: &[String]) -> alice_core::Result<Vec<Vec<f32>>> {
        self.inner.embed_passages(texts)
    }
    fn embed_query(&self, text: &str) -> alice_core::Result<Vec<f32>> {
        self.inner.embed_query(text)
    }
}
```

In `bench()`, replace the marker and `make` block with:

```rust
    let embedder: String = match (args.real, args.prefill_vectors) {
        (true, true) => format!("{}+prefill", alice_embed::MODEL_ID),
        (true, false) => alice_embed::MODEL_ID.into(),
        _ => "fake".into(),
    };
    match std::fs::read_to_string(&marker) {
        Ok(m) if m.trim() != embedder => bail!(
            "{} holds bench data for the {} embedder; use another --dir",
            home.root().display(),
            m.trim()
        ),
        Ok(_) => {}
        Err(_) => std::fs::write(&marker, &embedder)?,
    }
    let (real, prefill) = (args.real, args.prefill_vectors);
    let models = user_models.clone();
    let make = move || -> alice_core::Result<Arc<dyn Embedder>> {
        match (real, prefill) {
            (true, true) => Ok(Arc::new(Relabeled {
                inner: FakeEmbedder::new(alice_embed::DIM),
                id: alice_embed::MODEL_ID,
            })),
            (true, false) => Ok(Arc::new(alice_embed::FastEmbedder::new(&models)?)),
            _ => Ok(Arc::new(FakeEmbedder::new(384))),
        }
    };
```

The embedder-alone throughput probe must measure the real model in both real modes:

```rust
    let probe: Arc<dyn Embedder> = if real {
        Arc::new(alice_embed::FastEmbedder::new(&user_models)?)
    } else {
        make()?
    };
```

(`report.embedder = embedder.clone()`; `index_mb` now sums `memories.usearch` and `chunks.bq`.)

Add to `DaemonRun`:

```rust
    /// Memories written through the daemon after it started (real model only), and how long
    /// until the daemon had embedded all of them.
    remembered: usize,
    embed_new_ms: f64,
    rss_after_embedding_mb: Option<f64>,
    footprint_after_embedding_mb: Option<f64>,
```

In `daemon_run`, after `run.footprint_after_start_mb = …` and before the bursts:

```rust
        if real {
            // The model's working memory only grows when it embeds: write new memories
            // through the daemon and wait until its worker has embedded every one.
            const NEW: usize = 256;
            let t = Instant::now();
            let mut set = tokio::task::JoinSet::new();
            let texts: Vec<String> = (0..NEW).map(|_| corpus.memory_text()).collect();
            for chunk in texts.chunks(NEW / 8) {
                let (c, chunk) = (client.clone(), chunk.to_vec());
                set.spawn(async move {
                    for content in chunk {
                        c.remember(&RememberRequest {
                            content,
                            ..Default::default()
                        })
                        .await?;
                    }
                    anyhow::Ok(())
                });
            }
            while let Some(r) = set.join_next().await {
                r??;
            }
            while client.health().await?.embed_backlog > 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            run.remembered = NEW;
            run.embed_new_ms = ms(t.elapsed());
            run.rss_after_embedding_mb = rss_mb(pid);
            run.footprint_after_embedding_mb = footprint_mb(pid).map(|f| f.0);
        }
```

(`Corpus` already builds memory sentences for `load`; if its method has another name, expose the one `load` uses as `memory_text()`.) Add the four fields to `render`.

- [ ] **Step 3: Measure the baseline (before the new index)**

```bash
source "$HOME/.cargo/env"
cd /Users/icue/Code/AI/project-alice
export S=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2 ALICE_HOME=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2/home; mkdir -p "$ALICE_HOME"; [ -e "$ALICE_HOME/models" ] || ln -s "$HOME/.alice/models" "$ALICE_HOME/models"   # scratch prelude (Global Constraints)
nice -n 15 cargo build --release -j 6 -p alice-memory
cp target/release/alice $S/alice-base   # later tasks reuse $S and its data
nice -n 15 $S/alice-base bench --real --prefill-vectors --chunks 1000000 --memories 10000 \
  --dir $S/data-1m-prefill --out $S/base-1m.json
```

Expected: completes in roughly 10–20 minutes; `daemon.peak_rss_mb` / `peak_footprint_mb` well above 800 (M1 projected ~1.3–1.6 GB). Keep `$S/data-1m-prefill`: Step 12 reuses it (the database does not change; only the index files are rebuilt). Nothing else may run heavy work meanwhile.

- [ ] **Step 4: Write the failing tests for the sign-bit index**

Create `crates/alice-core/src/bq.rs` with only the test module first (and `pub mod bq;` in `lib.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{Embedder, FakeEmbedder, cosine};

    fn fake() -> FakeEmbedder {
        FakeEmbedder::new(384)
    }

    fn text(i: usize) -> String {
        format!("chunk {i} about topic {} and detail {}", i % 37, i % 101)
    }

    fn filled(n: usize) -> (BinaryIndex, Vec<Vec<f32>>) {
        let e = fake();
        let mut idx = BinaryIndex::new(384);
        let mut vs = Vec::new();
        for i in 0..n {
            let v = e.embed_query(&text(i)).unwrap();
            idx.upsert(i as i64 + 1, &v).unwrap();
            vs.push(v);
        }
        (idx, vs)
    }

    #[test]
    fn bootstrap_is_exact_then_codes_take_over_at_the_threshold() {
        let (idx, vs) = filled(BOOTSTRAP - 1);
        assert!(idx.is_bootstrapping());
        let c = idx.candidates(&vs[10], 1);
        assert_eq!(c[0].0, 11);
        assert!((c[0].1 - 1.0).abs() < 1e-5, "exact while bootstrapping");
        let (idx, vs) = filled(BOOTSTRAP + 10);
        assert!(!idx.is_bootstrapping());
        assert_eq!(idx.len(), BOOTSTRAP + 10);
        for probe in [0usize, 777, BOOTSTRAP + 5] {
            let c = idx.candidates(&vs[probe], 5);
            assert_eq!(c[0].0, probe as i64 + 1, "a stored vector is its own nearest code");
        }
    }

    #[test]
    fn candidates_contain_the_exact_top_ten_for_most_queries() {
        let (idx, vs) = filled(6_000);
        let e = fake();
        let mut found = 0;
        let queries = 50;
        for q in 0..queries {
            let qv = e.embed_query(&format!("topic {} and detail {}", q % 37, q % 101)).unwrap();
            let mut exact: Vec<(usize, f32)> =
                vs.iter().enumerate().map(|(i, v)| (i, cosine(&qv, v))).collect();
            exact.sort_by(|a, b| b.1.total_cmp(&a.1));
            let got: std::collections::HashSet<i64> =
                idx.candidates(&qv, 200).into_iter().map(|(s, _)| s).collect();
            found += exact[..10].iter().filter(|(i, _)| got.contains(&(*i as i64 + 1))).count();
        }
        let recall = found as f64 / (queries * 10) as f64;
        assert!(recall >= 0.95, "recall of the exact top 10 within 200 candidates: {recall:.3}");
    }

    #[test]
    fn upsert_replaces_inserts_out_of_order_and_remove_forgets() {
        let (mut idx, vs) = filled(BOOTSTRAP + 100);
        let e = fake();
        let other = e.embed_query("something completely different").unwrap();
        idx.upsert(5, &other).unwrap();
        assert_eq!(idx.len(), BOOTSTRAP + 100, "replace, not add");
        assert_eq!(idx.candidates(&other, 1)[0].0, 5);
        idx.upsert(-1, &vs[3]).unwrap_err();
        idx.upsert(1_000_000, &vs[7]).unwrap();
        assert!(idx.remove(1_000_000));
        assert!(!idx.remove(1_000_000));
        assert_eq!(idx.remove_many(&[1, 2, 3, 999_999]), 3);
        assert_eq!(idx.len(), BOOTSTRAP + 97);
        assert!(idx.candidates(&vs[0], 3).iter().all(|(s, _)| *s != 1));
        assert!(idx.upsert(9, &[0.0; 10]).is_err(), "wrong dimension");
    }

    #[test]
    fn save_and_load_round_trip_in_both_states_and_reject_damage() {
        for n in [100, BOOTSTRAP + 50] {
            let (idx, vs) = filled(n);
            let mut buf = Vec::new();
            idx.write_to(&mut buf).unwrap();
            let back = BinaryIndex::read_from(&buf[..], 384).unwrap();
            assert_eq!(back.len(), n);
            assert_eq!(back.is_bootstrapping(), idx.is_bootstrapping());
            assert_eq!(back.candidates(&vs[42], 3), idx.candidates(&vs[42], 3));
            assert!(BinaryIndex::read_from(&buf[..buf.len() - 9], 384).is_err(), "truncated");
            assert!(BinaryIndex::read_from(&buf[..], 768).is_err(), "other dimension");
            let mut bad = buf.clone();
            bad[0] = b'X';
            assert!(BinaryIndex::read_from(&bad[..], 384).is_err(), "bad magic");
        }
    }
}
```

- [ ] **Step 5: Run them to see them fail**

Run: `nice -n 15 cargo test -j 6 -p alice-core bq::`
Expected: FAIL to compile (`BinaryIndex` not defined).

- [ ] **Step 6: Implement `bq.rs`**

Put this above the test module:

```rust
//! Flat sign-bit index for raw chunks.
//!
//! Each vector is kept as one bit per dimension: whether it lies above the mean of the first
//! `BOOTSTRAP` vectors. Centring matters: e5 vectors share a large common direction (unrelated
//! texts sit at cosine ~0.7), so uncentred sign bits would be nearly constant. A search ranks
//! every code by Hamming distance (popcount of XOR) and returns the nearest candidates; the
//! caller rescores them with the exact vectors kept in SQLite (`VectorIndex::search_chunks`).
//!
//! Until `BOOTSTRAP` vectors have arrived there is no mean: those vectors are kept as f32 and
//! searched exactly. At `BOOTSTRAP` the mean is computed once and kept for the life of this
//! index; a rebuild computes a fresh one.
//!
//! Memory: 48 bytes of code and 8 bytes of key per 384-dimension vector (~56 MB at 1M chunks),
//! against ~560 MB for the int8 HNSW it replaces. Search is a linear scan (~48 MB at 1M).

use crate::error::{AliceError, Result};
use std::collections::BinaryHeap;
use std::io::{Read, Write};

/// Vectors kept exactly before the mean is fixed.
pub const BOOTSTRAP: usize = 4_096;
const MAGIC: &[u8; 8] = b"ALICEBQ1";

pub struct BinaryIndex {
    dim: usize,
    words: usize,
    mean: Option<Vec<f32>>,
    /// Before the mean is fixed: (seq, vector), sorted by seq.
    boot: Vec<(i64, Vec<f32>)>,
    /// After: sorted seqs, and `words` u64 of code per seq in the same order.
    keys: Vec<i64>,
    codes: Vec<u64>,
}

fn bad(msg: impl Into<String>) -> AliceError {
    AliceError::Other(format!("chunk index file: {}", msg.into()))
}

impl BinaryIndex {
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            words: dim.div_ceil(64),
            mean: None,
            boot: Vec::new(),
            keys: Vec::new(),
            codes: Vec::new(),
        }
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn len(&self) -> usize {
        self.keys.len() + self.boot.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_bootstrapping(&self) -> bool {
        self.mean.is_none()
    }

    pub fn upsert(&mut self, seq: i64, v: &[f32]) -> Result<()> {
        if v.len() != self.dim {
            return Err(AliceError::invalid(format!(
                "vector has {} dimensions; the index uses {}",
                v.len(),
                self.dim
            )));
        }
        if seq < 0 {
            return Err(AliceError::invalid(format!("chunk seq {seq} is negative")));
        }
        if self.mean.is_none() {
            match self.boot.binary_search_by_key(&seq, |(s, _)| *s) {
                Ok(i) => self.boot[i].1 = v.to_vec(),
                Err(i) => self.boot.insert(i, (seq, v.to_vec())),
            }
            if self.boot.len() >= BOOTSTRAP {
                self.fix_mean();
            }
            return Ok(());
        }
        self.put_code(seq, v);
        Ok(())
    }

    fn fix_mean(&mut self) {
        let mut mean = vec![0f32; self.dim];
        for (_, v) in &self.boot {
            for (m, x) in mean.iter_mut().zip(v) {
                *m += x;
            }
        }
        let n = self.boot.len().max(1) as f32;
        mean.iter_mut().for_each(|m| *m /= n);
        self.mean = Some(mean);
        let boot = std::mem::take(&mut self.boot);
        self.keys.reserve(boot.len());
        self.codes.reserve(boot.len() * self.words);
        for (seq, v) in boot {
            self.put_code(seq, &v);
        }
    }

    fn encode(&self, v: &[f32]) -> Vec<u64> {
        let mean = self.mean.as_deref().expect("the mean is fixed before encoding");
        let mut out = vec![0u64; self.words];
        for (i, (x, m)) in v.iter().zip(mean).enumerate() {
            if x > m {
                out[i / 64] |= 1u64 << (i % 64);
            }
        }
        out
    }

    fn put_code(&mut self, seq: i64, v: &[f32]) {
        let code = self.encode(v);
        let w = self.words;
        // Chunks arrive in seq order almost always: append.
        if self.keys.last().is_none_or(|&last| last < seq) {
            self.keys.push(seq);
            self.codes.extend_from_slice(&code);
            return;
        }
        match self.keys.binary_search(&seq) {
            Ok(i) => self.codes[i * w..(i + 1) * w].copy_from_slice(&code),
            Err(i) => {
                self.keys.insert(i, seq);
                self.codes.splice(i * w..i * w, code);
            }
        }
    }

    pub fn remove(&mut self, seq: i64) -> bool {
        self.remove_many(&[seq]) == 1
    }

    /// Remove every listed seq in one pass; returns how many were present.
    pub fn remove_many(&mut self, seqs: &[i64]) -> usize {
        let gone: std::collections::HashSet<i64> = seqs.iter().copied().collect();
        let before = self.len();
        self.boot.retain(|(s, _)| !gone.contains(s));
        let w = self.words;
        let mut keep = 0;
        for i in 0..self.keys.len() {
            if gone.contains(&self.keys[i]) {
                continue;
            }
            if keep != i {
                self.keys[keep] = self.keys[i];
                self.codes.copy_within(i * w..(i + 1) * w, keep * w);
            }
            keep += 1;
        }
        self.keys.truncate(keep);
        self.codes.truncate(keep * w);
        before - self.len()
    }

    /// Up to `n` seqs nearest to `query`, best first, with an estimated cosine similarity
    /// (`cos(π · hamming / dim)`, for ordering only; exact while bootstrapping).
    pub fn candidates(&self, query: &[f32], n: usize) -> Vec<(i64, f32)> {
        if n == 0 || query.len() != self.dim {
            return Vec::new();
        }
        if self.mean.is_none() {
            let mut all: Vec<(i64, f32)> = self
                .boot
                .iter()
                .map(|(s, v)| (*s, crate::embed::cosine(query, v)))
                .collect();
            all.sort_by(|a, b| b.1.total_cmp(&a.1));
            all.truncate(n);
            return all;
        }
        let q = self.encode(query);
        let w = self.words;
        let mut heap: BinaryHeap<(u32, usize)> = BinaryHeap::with_capacity(n + 1);
        for (i, code) in self.codes.chunks_exact(w).enumerate() {
            let d: u32 = code.iter().zip(&q).map(|(a, b)| (a ^ b).count_ones()).sum();
            if heap.len() < n {
                heap.push((d, i));
            } else if let Some(&(worst, _)) = heap.peek()
                && d < worst
            {
                heap.pop();
                heap.push((d, i));
            }
        }
        let mut out = heap.into_vec();
        out.sort_unstable();
        out.into_iter()
            .map(|(d, i)| {
                let est = (std::f32::consts::PI * d as f32 / self.dim as f32).cos();
                (self.keys[i], est)
            })
            .collect()
    }

    pub fn write_to(&self, mut w: impl Write) -> std::io::Result<()> {
        w.write_all(MAGIC)?;
        w.write_all(&(self.dim as u32).to_le_bytes())?;
        let boot = self.mean.is_none();
        w.write_all(&u32::from(boot).to_le_bytes())?;
        if boot {
            w.write_all(&(self.boot.len() as u64).to_le_bytes())?;
            for (s, v) in &self.boot {
                w.write_all(&s.to_le_bytes())?;
                for x in v {
                    w.write_all(&x.to_le_bytes())?;
                }
            }
        } else {
            w.write_all(&(self.keys.len() as u64).to_le_bytes())?;
            for x in self.mean.as_deref().unwrap_or_default() {
                w.write_all(&x.to_le_bytes())?;
            }
            for k in &self.keys {
                w.write_all(&k.to_le_bytes())?;
            }
            for c in &self.codes {
                w.write_all(&c.to_le_bytes())?;
            }
        }
        w.flush()
    }

    /// Fails on a wrong magic, another dimension, a truncated file or trailing bytes.
    pub fn read_from(mut r: impl Read, dim: usize) -> Result<Self> {
        fn take<const N: usize>(r: &mut impl Read) -> Result<[u8; N]> {
            let mut b = [0u8; N];
            r.read_exact(&mut b).map_err(|_| bad("truncated"))?;
            Ok(b)
        }
        if &take::<8>(&mut r)? != MAGIC {
            return Err(bad("not an Alice chunk index"));
        }
        let file_dim = u32::from_le_bytes(take::<4>(&mut r)?) as usize;
        if file_dim != dim {
            return Err(bad(format!("{file_dim} dimensions; {dim} expected")));
        }
        let boot = u32::from_le_bytes(take::<4>(&mut r)?) == 1;
        let n = u64::from_le_bytes(take::<8>(&mut r)?) as usize;
        let mut idx = BinaryIndex::new(dim);
        let f32s = |r: &mut dyn Read, count: usize| -> Result<Vec<f32>> {
            let mut buf = vec![0u8; count * 4];
            r.read_exact(&mut buf).map_err(|_| bad("truncated"))?;
            Ok(buf
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect())
        };
        if boot {
            for _ in 0..n {
                let s = i64::from_le_bytes(take::<8>(&mut r)?);
                idx.boot.push((s, f32s(&mut r, dim)?));
            }
        } else {
            idx.mean = Some(f32s(&mut r, dim)?);
            let mut keys = vec![0u8; n * 8];
            r.read_exact(&mut keys).map_err(|_| bad("truncated"))?;
            idx.keys = keys
                .as_chunks::<8>()
                .0
                .iter()
                .map(|b| i64::from_le_bytes(*b))
                .collect();
            let mut codes = vec![0u8; n * idx.words * 8];
            r.read_exact(&mut codes).map_err(|_| bad("truncated"))?;
            idx.codes = codes
                .as_chunks::<8>()
                .0
                .iter()
                .map(|b| u64::from_le_bytes(*b))
                .collect();
            if !idx.keys.is_sorted() {
                return Err(bad("keys out of order"));
            }
        }
        let mut rest = [0u8; 1];
        if r.read(&mut rest).map_err(|e| bad(e.to_string()))? != 0 {
            return Err(bad("trailing bytes"));
        }
        Ok(idx)
    }
}
```

- [ ] **Step 7: Run the tests**

Run: `nice -n 15 cargo test -j 6 -p alice-core bq::`
Expected: 4 passed. If `candidates_contain_the_exact_top_ten_for_most_queries` lands between 0.90 and 0.95 with the fake embedder, do not lower the threshold: raise the candidate count in that test only to 400 and note the measured value in the report (the real-model gate is Step 13).

- [ ] **Step 8: Write the failing `VectorIndex` and `get_many` tests**

In `crates/alice-core/src/store/embeddings.rs` tests:

```rust
    #[test]
    fn get_many_returns_stored_vectors_of_one_model_and_skips_missing() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        put(&c, VecKey::Chunk(1), "m", &[1.0, 0.0]).unwrap();
        put(&c, VecKey::Chunk(2), "m", &[0.0, 1.0]).unwrap();
        put(&c, VecKey::Chunk(2), "other", &[0.5, 0.5]).unwrap();
        let got = get_many(&c, "m", &[VecKey::Chunk(2), VecKey::Chunk(3), VecKey::Chunk(1)]).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], (VecKey::Chunk(2), vec![0.0, 1.0]));
        assert_eq!(got[1].0, VecKey::Chunk(1));
    }
```

In `crates/alice-core/src/vector.rs` tests (add these; then update every existing `VectorIndex::open(&p, 384)` in this file to `VectorIndex::open(&p, "fake", 384)`):

```rust
    /// The chunk part ranks by sign bits and `search_chunks` rescores with the stored vectors.
    #[test]
    fn chunk_search_rescores_candidates_with_the_stored_vectors() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let e = FakeEmbedder::new(384);
        let idx = VectorIndex::for_model("fake", 384).unwrap();
        for i in 1..=5_000i64 {
            let v = v(&e, &format!("chunk {i} on subject {} detail {}", i % 41, i % 97));
            crate::store::embeddings::put(&c, VecKey::Chunk(i), "fake", &v).unwrap();
            idx.upsert(VecKey::Chunk(i), &v).unwrap();
        }
        let q = v(&e, "chunk 1234 on subject 4 detail 70");
        let hits = idx.search_chunks(&c, &q, 10).unwrap();
        assert_eq!(hits[0].0, VecKey::Chunk(1234));
        assert!(hits[0].1 > 0.99, "exact cosine after rescoring: {}", hits[0].1);
        assert!(hits.windows(2).all(|w| w[0].1 >= w[1].1));
        assert_eq!(idx.len(), 5_000);
        // Without a model there are no stored vectors to rescore with.
        let anonymous = VectorIndex::new(384).unwrap();
        anonymous.upsert(VecKey::Chunk(1), &q).unwrap();
        assert!(anonymous.search_chunks(&c, &q, 5).unwrap().is_empty());
    }

    #[test]
    fn chunk_part_saves_reopens_and_deletes_the_m1_chunk_file() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("vectors").join("fake.usearch");
        std::fs::create_dir_all(base.parent().unwrap()).unwrap();
        let legacy = base.with_extension("chunks.usearch");
        std::fs::write(&legacy, b"old hnsw").unwrap();
        let e = FakeEmbedder::new(384);
        let idx = VectorIndex::for_model("fake", 384).unwrap();
        for i in 1..=50i64 {
            idx.upsert(VecKey::Chunk(i), &v(&e, &format!("c{i}"))).unwrap();
        }
        idx.upsert(VecKey::Memory(7), &v(&e, "m7")).unwrap();
        idx.save(&base).unwrap();
        assert!(!legacy.exists(), "the M1 chunk HNSW file is removed");
        assert!(base.with_extension("chunks.bq").exists());
        let back = VectorIndex::open(&base, "fake", 384).unwrap();
        assert_eq!(back.len(), 51);
        assert_eq!(back.model(), "fake");
        assert!(!back.is_dirty());
    }

    #[test]
    fn rebuild_fills_both_parts_from_the_table() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let e = FakeEmbedder::new(384);
        for i in 1..=4_200i64 {
            crate::store::embeddings::put(&c, VecKey::Chunk(i), "fake", &v(&e, &format!("c{i}"))).unwrap();
        }
        crate::store::embeddings::put(&c, VecKey::Memory(3), "fake", &v(&e, "m")).unwrap();
        let idx = VectorIndex::rebuild_from(&c, "fake", 384).unwrap();
        assert_eq!(idx.len(), 4_201);
        let q = v(&e, "c4100");
        assert_eq!(idx.search_chunks(&c, &q, 1).unwrap()[0].0, VecKey::Chunk(4100));
    }
```

**Cross-task edit (M1 test):** `vector.rs`'s M1 test `save_writes_two_files_and_open_needs_both` hard-codes the old chunk file and the old `open`: build its index with `VectorIndex::for_model("m", 384)` (an anonymous index has no model to rescore with), expect `dir.join("m.chunks.bq")` instead of `m.chunks.usearch`, call `VectorIndex::open(&path, "m", 384)`, and read the chunk count through the new chunk part's `len()`; its "open needs both" half removes `m.chunks.bq`. Search `vector.rs`, `service.rs` and `worker.rs` tests for any other `chunks.usearch` or two-argument `open(`.

Run: `nice -n 15 cargo test -j 6 -p alice-core vector:: embeddings::`
Expected: FAIL to compile (`for_model`, `search_chunks`, `get_many`, `open` arity).

- [ ] **Step 9: Implement `get_many` and the chunk part in `vector.rs`**

`crates/alice-core/src/store/embeddings.rs` (make `VecKey::parts` `pub(crate)`; add `OptionalExtension` to the file's `use rusqlite::{Connection, params};`):

```rust
/// Stored vectors of `model` for `keys`, in the order given; keys without one are left out.
/// One primary-key lookup per key (a recall rescores a few hundred chunk candidates).
pub fn get_many(conn: &Connection, model: &str, keys: &[VecKey]) -> Result<Vec<(VecKey, Vec<f32>)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT vector FROM embeddings WHERE owner_type = ?1 AND owner_seq = ?2 AND model = ?3",
    )?;
    let mut out = Vec::with_capacity(keys.len());
    for &key in keys {
        let (t, s) = key.parts();
        if let Some(blob) = stmt
            .query_row(params![t, s, model], |r| r.get::<_, Vec<u8>>(0))
            .optional()?
        {
            out.push((key, decode_f16(&blob)));
        }
    }
    Ok(out)
}
```

In `vector.rs`, add next to `Part` a chunk part with the same locking and save rules:

```rust
/// The chunk part: a sign-bit index (see `crate::bq`) with the same locks and save rules as `Part`.
struct ChunkPart {
    inner: RwLock<BinaryIndex>,
    writer: Mutex<()>,
    dirty: AtomicBool,
    synced: Mutex<Option<PathBuf>>,
}

impl ChunkPart {
    fn new(dim: usize) -> Self {
        Self {
            inner: RwLock::new(BinaryIndex::new(dim)),
            writer: Mutex::new(()),
            dirty: AtomicBool::new(false),
            synced: Mutex::new(None),
        }
    }

    fn open(path: &Path, dim: usize) -> Result<Self> {
        let f = std::fs::File::open(path).map_err(|e| {
            AliceError::NotFound(format!("chunk index {}: {e}", path.display()))
        })?;
        let idx = BinaryIndex::read_from(std::io::BufReader::new(f), dim)?;
        let part = Self::new(dim);
        *part.inner.write() = idx;
        *part.synced.lock() = Some(path.to_path_buf());
        Ok(part)
    }

    fn put(&self, items: &[(i64, &[f32])]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let _writer = self.writer.lock();
        let mut i = self.inner.write();
        for &(seq, v) in items {
            i.upsert(seq, v)?;
        }
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn remove_many(&self, seqs: &[i64]) -> usize {
        let _writer = self.writer.lock();
        let n = self.inner.write().remove_many(seqs);
        if n > 0 {
            self.dirty.store(true, Ordering::Relaxed);
        }
        n
    }

    fn candidates(&self, query: &[f32], n: usize) -> Vec<(i64, f32)> {
        self.inner.read().candidates(query, n)
    }

    fn len(&self) -> usize {
        self.inner.read().len()
    }

    fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Atomic: temp file, sync, rename. Skipped when unchanged since the last open/save of `path`.
    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _writer = self.writer.lock();
        if !self.is_dirty() && self.synced.lock().as_deref() == Some(path) && path.exists() {
            return Ok(());
        }
        let tmp = path.with_extension("bq.tmp");
        let written = (|| -> Result<()> {
            let f = std::fs::File::create(&tmp)?;
            let mut w = std::io::BufWriter::new(f);
            self.inner.read().write_to(&mut w)?;
            let f = w.into_inner().map_err(|e| AliceError::Io(e.into_error()))?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)?;
            Ok(())
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written?;
        self.dirty.store(false, Ordering::Relaxed);
        *self.synced.lock() = Some(path.to_path_buf());
        Ok(())
    }
}
```

Change `VectorIndex` (keep `Part` for memories):

```rust
/// Chunk candidates rescored per result wanted (and never fewer than `MIN_RESCORE`).
pub const CHUNK_OVERSAMPLE: usize = 4;
pub const MIN_RESCORE: usize = 200;

pub struct VectorIndex {
    dim: usize,
    /// The model whose stored vectors rescore chunk candidates ("" = none: no chunk results).
    model: String,
    /// Even keys only.
    memories: Part,
    chunks: ChunkPart,
}

fn part_paths(path: &Path) -> (PathBuf, PathBuf) {
    (
        path.with_extension("memories.usearch"),
        path.with_extension("chunks.bq"),
    )
}

/// The M1 chunk HNSW, replaced by `chunks.bq`.
fn legacy_chunk_path(path: &Path) -> PathBuf {
    path.with_extension("chunks.usearch")
}
```

and in `impl VectorIndex`: `new(dim)` → `Self::for_model("", dim)`; `with_threads(dim, threads)` takes the model too; add

```rust
    pub fn for_model(model: &str, dim: usize) -> Result<Self> {
        Self::with_threads(model, dim, THREADS)
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn open(path: &Path, model: &str, dim: usize) -> Result<Self> {
        let (memories, chunks) = part_paths(path);
        Ok(Self {
            dim,
            model: model.to_string(),
            memories: Part::open(&memories, dim)?,
            chunks: ChunkPart::open(&chunks, dim)?,
        })
    }
```

`put` sends chunk items to `self.chunks.put(&[(seq, v)…])` (checked keys as before); `remove` of a chunk calls `self.chunks.remove_many(&[seq])`; add

```rust
    /// Remove many chunk vectors at once (retention, purges); returns how many were present.
    pub fn remove_chunks(&self, seqs: &[i64]) -> Result<usize> {
        Ok(self.chunks.remove_many(seqs))
    }

    /// Nearest chunks by exact cosine: sign-bit candidates (`k × CHUNK_OVERSAMPLE`, at least
    /// `MIN_RESCORE`) rescored with this model's f16 vectors from the `embeddings` table.
    pub fn search_chunks(&self, conn: &Connection, query: &[f32], k: usize) -> Result<Vec<(VecKey, f32)>> {
        self.search_chunks_with(conn, query, k, CHUNK_OVERSAMPLE)
    }

    pub fn search_chunks_with(
        &self,
        conn: &Connection,
        query: &[f32],
        k: usize,
        oversample: usize,
    ) -> Result<Vec<(VecKey, f32)>> {
        check_dim("query", query.len(), self.dim)?;
        if k == 0 || self.model.is_empty() {
            return Ok(Vec::new());
        }
        let n = (k * oversample.max(1)).max(MIN_RESCORE);
        let keys: Vec<VecKey> = self
            .chunks
            .candidates(query, n)
            .into_iter()
            .map(|(s, _)| VecKey::Chunk(s))
            .collect();
        let mut scored: Vec<(VecKey, f32)> = embeddings::get_many(conn, &self.model, &keys)?
            .into_iter()
            .map(|(key, v)| (key, crate::embed::cosine(query, &v)))
            .collect();
        // Best first; ties go to the newer chunk.
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.0.cmp(&a.0)));
        scored.truncate(k);
        Ok(scored)
    }
```

`search(.., KeyFilter::Chunks)` returns `self.chunks.candidates(query, k)` mapped to `VecKey::Chunk` (document: *estimated* similarity, ordering only; recall uses `search_chunks`); `Any` merges as before. `len`, `is_dirty` add the chunk part. `save(path)` saves memories, then chunks, then `let _ = std::fs::remove_file(legacy_chunk_path(path));`. `rebuild_from(conn, model, dim)` starts from `Self::for_model(model, dim)`; its loop is unchanged (`upsert` routes chunks to the new part). Update the module docs at the top of `vector.rs` (two parts: memories HNSW, chunks sign bits + rescoring).

- [ ] **Step 10: Use it in recall and the service**

`crates/alice-core/src/search/mod.rs`, in `Query::raw`, replace the vector list:

```rust
        let vector = match (self.index, self.vector.as_deref()) {
            (Some(index), Some(v)) => index
                .search_chunks(self.conn, v, self.k)
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "chunk vector search failed; using word matches only");
                    Vec::new()
                }),
            _ => Vec::new(),
        }
        .into_iter()
        .filter_map(|(key, sim)| match key {
            VecKey::Chunk(s) => Some((ItemRef::Chunk(s), sim)),
            VecKey::Memory(_) => None,
        })
        .collect();
```

Add a test in `search/mod.rs` tests (its fixtures already ingest chunks; use their helper names): ingest one chunk, store its fake vector with `embeddings::put(&c, VecKey::Chunk(seq), "fake", &v)`, build `VectorIndex::for_model("fake", 384)` holding it, run `run(..)` with `include_raw: true` and a query whose fake vector is close to the chunk's, and assert the raw hit's `matched` contains `MatchVia::Vector`.

`crates/alice-daemon/src/service.rs`: `VectorIndex::open(&vector_path, &model, dim)`; in `spawn_rebuild`'s error branch `VectorIndex::for_model(&model, dim)`. In `restart_keeps_data_and_rebuilds_a_missing_index` the removed files become `["memories.usearch", "chunks.bq"]`. `crates/alice-daemon/src/worker.rs` tests: `VectorIndex::open(&r.vector_path(), "fake", 16)`. `crates/alice/src/bench.rs` `startup_parts`: `VectorIndex::open(&home.vector_file(model), model, dim)`. `crates/alice-daemon/src/paths.rs`: the `vector_file` doc names `<model>.memories.usearch` and `<model>.chunks.bq`.

Run: `nice -n 15 cargo test -j 6 --workspace`
Expected: all pass (the M1 suite plus the new tests).

- [ ] **Step 11: Real-model recall gate for the chunk index**

Create `crates/alice-embed/tests/binary_chunk_recall.rs`:

```rust
//! Recall of the sign-bit chunk index against exact cosine, with the real model, on text from
//! this repository (Rust code, English and Thai docs). For each query (the first 60% of a
//! passage) the ground truth is the exact cosine top 10 over every passage; recall@10 is the
//! share of it that `search_chunks_with` returns. Prints recall per oversampling factor and
//! asserts it at `CHUNK_OVERSAMPLE`.
//!
//!   ALICE_MODELS_DIR=~/.alice/models cargo test -p alice-embed --release \
//!     --test binary_chunk_recall -- --ignored --nocapture
#![cfg(feature = "local")]

use alice_core::embed::{Embedder, cosine};
use alice_core::store::embeddings::{self, VecKey};
use alice_core::vector::{CHUNK_OVERSAMPLE, VectorIndex};
use std::path::{Path, PathBuf};

fn models_dir() -> PathBuf {
    std::env::var_os("ALICE_MODELS_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".alice/models")))
        .expect("set ALICE_MODELS_DIR")
}

/// Lines of 40..400 characters from the repo's docs and sources, deduplicated, in file order.
fn passages(root: &Path, max: usize) -> Vec<String> {
    let mut files = Vec::new();
    for dir in ["docs", "crates"] {
        let mut stack = vec![root.join(dir)];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() && !p.ends_with("target") {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "md" || x == "rs") {
                    files.push(p);
                }
            }
        }
    }
    files.push(root.join("README.md"));
    files.push(root.join("README.th.md"));
    files.sort();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for f in files {
        for line in std::fs::read_to_string(&f).unwrap_or_default().lines() {
            let t = line.trim();
            let n = t.chars().count();
            if (40..=400).contains(&n) && seen.insert(t.to_string()) {
                out.push(t.to_string());
                if out.len() == max {
                    return out;
                }
            }
        }
    }
    out
}

#[test]
#[ignore = "needs the real model in ALICE_MODELS_DIR or ~/.alice/models"]
fn sign_bit_chunk_index_keeps_the_exact_top_ten() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let texts = passages(&root, 6_000);
    assert!(texts.len() >= 3_000, "only {} passages", texts.len());
    let e = alice_embed::FastEmbedder::new(&models_dir()).unwrap();
    let model = e.model_id().to_string();
    let vectors: Vec<Vec<f32>> = texts
        .chunks(32)
        .flat_map(|b| e.embed_passages(b).unwrap())
        .collect();
    let d = tempfile::tempdir().unwrap();
    let c = alice_core::store::open_writer(&d.path().join("a.db")).unwrap();
    let idx = VectorIndex::for_model(&model, e.dim()).unwrap();
    for (i, v) in vectors.iter().enumerate() {
        let key = VecKey::Chunk(i as i64 + 1);
        embeddings::put(&c, key, &model, v).unwrap();
        idx.upsert(key, v).unwrap();
    }
    let queries: Vec<(String, Vec<f32>)> = texts
        .iter()
        .step_by(texts.len() / 300)
        .map(|t| {
            let cut: String = t.chars().take(t.chars().count() * 6 / 10).collect();
            let v = e.embed_query(&cut).unwrap();
            (cut, v)
        })
        .collect();
    let mut report = String::new();
    let mut at_default = 0.0;
    for oversample in [1usize, 2, 4, 8, 16] {
        let mut found = 0usize;
        for (_, q) in &queries {
            let mut exact: Vec<(i64, f32)> = vectors
                .iter()
                .enumerate()
                .map(|(i, v)| (i as i64 + 1, cosine(q, v)))
                .collect();
            exact.sort_by(|a, b| b.1.total_cmp(&a.1));
            let got: std::collections::HashSet<VecKey> = idx
                .search_chunks_with(&c, q, 10, oversample)
                .unwrap()
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            found += exact[..10]
                .iter()
                .filter(|(s, _)| got.contains(&VecKey::Chunk(*s)))
                .count();
        }
        let recall = found as f64 / (queries.len() * 10) as f64;
        report.push_str(&format!("oversample {oversample:>2}: recall@10 {recall:.4}\n"));
        if oversample == CHUNK_OVERSAMPLE {
            at_default = recall;
        }
    }
    println!(
        "{} passages, {} queries\n{report}",
        texts.len(),
        queries.len()
    );
    assert!(
        at_default >= 0.97,
        "recall@10 {at_default:.4} at CHUNK_OVERSAMPLE {CHUNK_OVERSAMPLE}"
    );
}
```

(`MIN_RESCORE` = 200 dominates for k = 10, so the factor matters only above 20; the sweep still shows the candidate count needed. Add `tempfile` is already a dev-dependency of `alice-embed`.)

Run (the model is in `~/.alice/models`; read-only):

```bash
ALICE_MODELS_DIR=$HOME/.alice/models nice -n 15 cargo test -j 6 -p alice-embed --release \
  --test binary_chunk_recall -- --ignored --nocapture
ALICE_MODELS_DIR=$HOME/.alice/models nice -n 15 cargo test -j 6 -p alice-embed --release \
  --test real_search_quality -- --ignored --nocapture
nice -n 15 cargo test -j 6 -p alice-core --test search_quality
```

Expected: recall@10 ≥ 0.97 at the default; both gold tests pass with unchanged thresholds. **Decision rule if recall is below 0.97:** raise `MIN_RESCORE` (not the gate) to the smallest of 400, 800, 1,600 that reaches it, re-run both gold tests and Step 12's latency numbers; record the choice in `docs/benchmarks/m2.md`.

- [ ] **Step 12: Re-measure at 1M with the new index**

```bash
export S=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2 ALICE_HOME=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2/home; mkdir -p "$ALICE_HOME"; [ -e "$ALICE_HOME/models" ] || ln -s "$HOME/.alice/models" "$ALICE_HOME/models"   # scratch prelude (Global Constraints)
nice -n 15 cargo build --release -j 6 -p alice-memory && cp target/release/alice $S/alice-bq
nice -n 15 $S/alice-bq bench --real --prefill-vectors --chunks 1000000 --memories 10000 \
  --dir $S/data-1m-prefill --out $S/bq-1m.json
```

Expected: the run reuses the data, rebuilds the chunk part from the table (start-up parts show a rebuild of seconds instead of 333 s), `daemon.peak_rss_mb` drops by roughly 400–500 MB against `base-1m.json`; recall p95 (`latency.recall_p95_ms`, `recall_raw_p95_ms`) stays under 200 ms. Run `pgrep -fl "alice daemon run"` afterwards: no bench daemon is left.

- [ ] **Step 13: Write `docs/benchmarks/m2.md` (first sections)**

Create it with: machine and load (as `m1.md` does), the command lines of Steps 3 and 12, a table "Daemon memory at 1M chunks with the real model" with columns *M1 (int8 HNSW chunks)* and *sign-bit chunks* and rows RSS after start / after embedding / after bursts / peak, footprint after start / after embedding / after bursts / peak, index files MB, chunk index rebuild time, recall p95, recall `include_raw` p95; a section "Chunk recall of the sign-bit index" with Step 11's printed table and the oversampling decision; a line saying Task 2 continues the memory work. Keep `$S` paths out of the doc.

- [ ] **Step 14: Commit**

```bash
nice -n 15 cargo fmt --all && nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
git add crates/alice-core/src/bq.rs crates/alice-core/src/lib.rs crates/alice-core/src/vector.rs \
  crates/alice-core/src/store/embeddings.rs crates/alice-core/src/search/mod.rs \
  crates/alice-daemon/src/service.rs crates/alice-daemon/src/worker.rs crates/alice-daemon/src/paths.rs \
  crates/alice/src/cmd.rs crates/alice/src/bench.rs crates/alice-embed/tests/binary_chunk_recall.rs \
  docs/benchmarks/m2.md
git commit -m "perf: sign-bit chunk index rescored from SQLite; bench --prefill-vectors for real-model 1M runs

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: Memory budget II — ONNX Runtime embedder with a compact tokenizer, idle unload; budget verified

One fastembed instance is ~600 MB: the Hugging Face `tokenizers` Unigram builds a trie of one hash map per node for the 250k-piece XLM-R vocabulary (~250 MB), the ONNX session ~265 MB, and ONNX Runtime's CPU arena keeps the working memory of its largest call. This task replaces fastembed in the daemon with a direct ONNX Runtime session (`ort` =2.0.0-rc.13, the version fastembed already links), a Unigram tokenizer that keeps the vocabulary in one hash map, the CPU arena and memory patterns turned off, and an optional unload of the model after a configurable idle time (queries fall back to word matches while it reloads). The vectors stay those of M1 (checked against fastembed), so stored embeddings remain valid and the model id stays `multilingual-e5-small-q8`. The task ends with the 800 MB budget measured at 1M chunks.

**Files:**
- Create: `crates/alice-embed/src/tokenizer.rs`, `crates/alice-embed/src/lazy.rs`, `crates/alice-embed/src/onnx.rs`
- Modify: `crates/alice-embed/src/lib.rs` (drop the fastembed `local` module; `pub mod tokenizer; mod lazy; pub mod onnx;`, all three behind feature `local`; re-export `OnnxEmbedder`, `OnnxOptions`)
- Modify: `crates/alice-embed/Cargo.toml`, root `Cargo.toml` (deps)
- Modify: `crates/alice-core/src/embed.rs` (`Embedder::is_loaded` with a default)
- Modify (only if Step 9's fallback (1) or (2) is needed): `crates/alice-core/src/store/mod.rs` (`READER_CACHE_KIB`, `WRITER_CACHE_KIB`)
- Modify: `crates/alice-daemon/src/daemon.rs` (`OnnxEmbedder` with a 10-minute idle unload), `crates/alice/src/bench.rs`, `crates/alice-embed/tests/{real_model.rs,real_search_quality.rs,binary_chunk_recall.rs}` (use `OnnxEmbedder`)
- Create: `crates/alice-embed/tests/onnx_equivalence.rs` (ignored, real model)
- Modify: `docs/benchmarks/m2.md`

**Interfaces:**
- Consumes: Task 1's bench `--prefill-vectors`; `alice_embed::{MODEL_ID, DIM, model_dir, is_model_present}`.
- Produces:
  - `alice_embed::tokenizer::UnigramTokenizer::{from_file(path: &Path) -> Result<Self>, from_json(bytes: &[u8]) -> Result<Self>, encode(&self, text: &str, max_len: usize) -> Vec<u32>, normalize(&self, text: &str) -> String}`
  - `alice_embed::onnx::{OnnxEmbedder, OnnxOptions { idle_unload: Option<Duration>, intra_threads: Option<usize> }}`; `OnnxEmbedder::new(models_dir: &Path, opts: OnnxOptions) -> Result<Self>`; `impl Embedder` with `model_id() = "multilingual-e5-small-q8"`, `dim() = 384`; `embed_query` returns `Err(AliceError::Other(MODEL_LOADING))` while an unloaded model reloads (search then uses word matches); `embed_passages` waits for the load.
  - `alice_embed::onnx::MODEL_LOADING: &str`
  - `alice_core::embed::Embedder::is_loaded(&self) -> bool` (default `true`).
  - `alice_embed::FastEmbedder` no longer exists; `fastembed` is a dev-dependency only (equivalence tests).

- [ ] **Step 1: Read the files this task modifies, as they are now.**

- [ ] **Step 2: Dependencies**

Root `Cargo.toml` `[workspace.dependencies]` add:

```toml
ort = { version = "=2.0.0-rc.13", default-features = false, features = ["std", "api-24", "download-binaries", "tls-rustls"] }
spm_precompiled = "0.1.4"
```

`crates/alice-embed/Cargo.toml`:

```toml
[features]
default = ["local"]
local = ["dep:ort", "dep:spm_precompiled"]

[dependencies]
alice-core = { path = "../alice-core" }
ort = { workspace = true, optional = true }
spm_precompiled = { workspace = true, optional = true }
serde = { workspace = true }
serde_json = { workspace = true }
reqwest.workspace = true
rustls.workspace = true
futures.workspace = true
sha2.workspace = true
hex.workspace = true
parking_lot.workspace = true
tokio.workspace = true
tracing.workspace = true

[dev-dependencies]
tempfile.workspace = true
chrono.workspace = true
fastembed.workspace = true   # equivalence tests only
```

(`serde_json` moves from dev- to normal dependencies. The feature names `download-binaries`/`tls-rustls` are those fastembed's `ort-download-binaries-rustls-tls` enables; `cargo tree -p alice-embed -e features | grep ort` must show one `ort` version.)

- [ ] **Step 3: Write the failing tokenizer tests (synthetic vocabulary, no model needed)**

`crates/alice-embed/src/tokenizer.rs`, test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny Unigram vocabulary in the shape of the real tokenizer.json (no Precompiled
    /// normalizer: identity), so Viterbi, Metaspace, unknowns and truncation are tested offline.
    fn tiny() -> UnigramTokenizer {
        let json = serde_json::json!({
            "normalizer": {"type": "Sequence", "normalizers": [
                {"type": "Replace", "pattern": {"Regex": " {2,}"}, "content": " "}
            ]},
            "pre_tokenizer": {"type": "Metaspace", "replacement": "▁", "add_prefix_space": true},
            "model": {"type": "Unigram", "unk_id": 3, "vocab": [
                ["<s>", 0.0], ["<pad>", 0.0], ["</s>", 0.0], ["<unk>", 0.0],
                ["▁", -2.0], ["▁hello", -3.0], ["▁hell", -4.0], ["o", -4.5],
                ["▁world", -3.5], ["h", -6.0], ["e", -6.0], ["l", -6.0], ["w", -6.0],
                ["r", -6.0], ["d", -6.0], ["▁ไทย", -5.0], ["ไ", -7.0], ["ท", -7.0], ["ย", -7.0]
            ]}
        });
        UnigramTokenizer::from_json(json.to_string().as_bytes()).unwrap()
    }

    #[test]
    fn viterbi_prefers_the_best_scoring_split_and_wraps_in_specials() {
        let t = tiny();
        // ▁hello (-3) beats ▁hell + o (-8.5).
        assert_eq!(t.encode("hello world", 512), vec![0, 5, 8, 2]);
        // Runs of spaces collapse; a leading space does not add a second ▁.
        assert_eq!(t.encode("  hello    world", 512), vec![0, 5, 8, 2]);
        assert_eq!(t.encode("ไทย", 512), vec![0, 15, 2]);
    }

    #[test]
    fn unknown_characters_become_one_fused_unk_per_run() {
        let t = tiny();
        // "▁" then "zz" unknown (fused into one <unk>), then "o".
        assert_eq!(t.encode("zzo", 512), vec![0, 4, 3, 7, 2]);
        // Two runs separated by a known char stay two unks.
        assert_eq!(t.encode("zozz", 512), vec![0, 4, 3, 7, 3, 2]);
    }

    #[test]
    fn literal_special_tokens_are_text_and_truncation_keeps_both_ends() {
        let t = tiny();
        let ids = t.encode("</s>", 512);
        assert_eq!(ids.first(), Some(&0));
        assert_eq!(ids.last(), Some(&2));
        assert!(ids[1..ids.len() - 1].iter().all(|&i| i != 2), "{ids:?}");
        let long = "hello ".repeat(600);
        let ids = t.encode(&long, 512);
        assert_eq!(ids.len(), 512);
        assert_eq!((ids[0], ids[511]), (0, 2));
    }

    #[test]
    fn a_non_unigram_file_is_refused_with_a_next_step() {
        let json = r#"{"model": {"type": "WordPiece", "vocab": {}}}"#;
        let err = UnigramTokenizer::from_json(json.as_bytes()).err().unwrap().to_string();
        assert!(err.contains("alice model fetch"), "{err}");
    }
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-embed tokenizer::` — Expected: FAIL to compile.

- [ ] **Step 4: Implement the tokenizer**

Above the tests in `tokenizer.rs`:

```rust
//! The XLM-R Unigram tokenizer of multilingual-e5, without the Hugging Face `tokenizers` trie.
//!
//! `tokenizers` builds one hash map per trie node for the 250k-piece vocabulary (~250 MB
//! resident). This keeps the pieces in one hash map (~15 MB) and tries every prefix length at
//! each position, which builds the same lattice. It reproduces tokenizers 0.23 on this model's
//! `tokenizer.json` (checked by `tests/onnx_equivalence.rs`): the Precompiled (nmt_nfkc)
//! normalizer, runs of spaces collapsed, Metaspace (`▁` replaces spaces and is prepended; words
//! start at each `▁`), Unigram Viterbi with ties kept by the first path found, unknown
//! characters scored `min_score - 10` and consecutive unknowns in a word fused into one,
//! `<s> … </s>` around the ids, and truncation to `max_len` ids including those two.
//! Literal special-token text such as `</s>` in a passage is treated as text (Hugging Face
//! would emit the special id there).

use alice_core::error::{AliceError, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

const UNK_PENALTY: f64 = 10.0;
const SPECIALS: [&str; 5] = ["<s>", "<pad>", "</s>", "<unk>", "<mask>"];

pub struct UnigramTokenizer {
    pieces: HashMap<Box<str>, u32>,
    scores: Vec<f64>,
    unk_id: u32,
    unk_score: f64,
    max_piece_chars: usize,
    precompiled: Option<spm_precompiled::Precompiled>,
    bos: u32,
    eos: u32,
}

#[derive(Deserialize)]
struct FileJson {
    #[serde(default)]
    normalizer: Option<serde_json::Value>,
    model: ModelJson,
}

#[derive(Deserialize)]
struct ModelJson {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    unk_id: Option<u32>,
    #[serde(default)]
    vocab: serde_json::Value,
}

fn broken(why: impl std::fmt::Display) -> AliceError {
    AliceError::Other(format!(
        "the tokenizer file cannot be used ({why}); run \"alice model fetch\" to repair the model files"
    ))
}

/// The Precompiled normalizer inside `normalizer` (alone or in a Sequence), if any.
fn find_precompiled(v: Option<&serde_json::Value>) -> Result<Option<spm_precompiled::Precompiled>> {
    let Some(v) = v else { return Ok(None) };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("Precompiled") => serde_json::from_value(v.clone()).map(Some).map_err(broken),
        Some("Sequence") => {
            for n in v["normalizers"].as_array().into_iter().flatten() {
                if let Some(p) = find_precompiled(Some(n))? {
                    return Ok(Some(p));
                }
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

impl UnigramTokenizer {
    pub fn from_file(path: &Path) -> Result<Self> {
        Self::from_json(&std::fs::read(path)?)
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let f: FileJson = serde_json::from_slice(bytes).map_err(broken)?;
        if f.model.kind != "Unigram" {
            return Err(broken(format!("a {} model, not Unigram", f.model.kind)));
        }
        let vocab: Vec<(String, f64)> = serde_json::from_value(f.model.vocab).map_err(broken)?;
        let precompiled = find_precompiled(f.normalizer.as_ref())?;
        let mut pieces = HashMap::with_capacity(vocab.len());
        let mut scores = Vec::with_capacity(vocab.len());
        let (mut max_chars, mut min_score) = (1usize, f64::INFINITY);
        let mut special_ids = HashMap::new();
        for (i, (piece, score)) in vocab.into_iter().enumerate() {
            let id = i as u32;
            scores.push(score);
            min_score = min_score.min(score);
            if SPECIALS.contains(&piece.as_str()) {
                special_ids.insert(piece, id);
                continue; // never matched from text
            }
            max_chars = max_chars.max(piece.chars().count());
            pieces.insert(piece.into_boxed_str(), id);
        }
        let id_of = |name: &str| {
            special_ids
                .get(name)
                .copied()
                .ok_or_else(|| broken(format!("no {name} token")))
        };
        let unk_id = match f.model.unk_id {
            Some(id) => id,
            None => id_of("<unk>")?,
        };
        Ok(Self {
            pieces,
            scores,
            unk_id,
            unk_score: min_score - UNK_PENALTY,
            max_piece_chars: max_chars,
            precompiled,
            bos: id_of("<s>")?,
            eos: id_of("</s>")?,
        })
    }

    /// Normalizer and Metaspace: the string whose `▁`-led words the model encodes.
    pub fn normalize(&self, text: &str) -> String {
        let s = match &self.precompiled {
            Some(p) => p.normalize_string(text),
            None => text.to_string(),
        };
        let mut out = String::with_capacity(s.len() + 3);
        let mut prev_space = false;
        for c in s.chars() {
            if c == ' ' {
                if !prev_space {
                    out.push('▁');
                }
                prev_space = true;
            } else {
                out.push(c);
                prev_space = false;
            }
        }
        if !out.starts_with('▁') {
            out.insert(0, '▁');
        }
        out
    }

    /// `normalize` always starts the string with `▁`, so every word starts at a `▁`.
    pub fn encode(&self, text: &str, max_len: usize) -> Vec<u32> {
        let normalized = self.normalize(text);
        let starts: Vec<usize> = normalized
            .char_indices()
            .filter(|(_, c)| *c == '▁')
            .map(|(i, _)| i)
            .collect();
        let mut ids = vec![self.bos];
        for (n, &start) in starts.iter().enumerate() {
            let end = starts.get(n + 1).copied().unwrap_or(normalized.len());
            self.encode_word(&normalized[start..end], &mut ids);
            if ids.len() >= max_len {
                break;
            }
        }
        ids.truncate(max_len.saturating_sub(1).max(1));
        ids.push(self.eos);
        ids
    }

    fn encode_word(&self, word: &str, out: &mut Vec<u32>) {
        let bounds: Vec<usize> = word
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(word.len()))
            .collect();
        let n = bounds.len() - 1;
        // best[i] = (score, start, id) of the best path ending at char i.
        let mut best: Vec<Option<(f64, usize, u32)>> = vec![None; n + 1];
        best[0] = Some((0.0, 0, 0));
        for start in 0..n {
            let Some((base, _, _)) = best[start] else { continue };
            let mut has_single = false;
            for len in 1..=self.max_piece_chars.min(n - start) {
                let end = start + len;
                if let Some(&id) = self.pieces.get(&word[bounds[start]..bounds[end]]) {
                    let score = base + self.scores[id as usize];
                    if best[end].is_none_or(|(s, _, _)| score > s) {
                        best[end] = Some((score, start, id));
                    }
                    has_single |= len == 1;
                }
            }
            if !has_single {
                let score = base + self.unk_score;
                if best[start + 1].is_none_or(|(s, _, _)| score > s) {
                    best[start + 1] = Some((score, start, self.unk_id));
                }
            }
        }
        let mut ids = Vec::new();
        let mut pos = n;
        while pos > 0 {
            let (_, start, id) = best[pos].expect("every char boundary is reachable");
            ids.push(id);
            pos = start;
        }
        ids.reverse();
        let mut prev_unk = false;
        for id in ids {
            let unk = id == self.unk_id;
            if !(unk && prev_unk) {
                out.push(id);
            }
            prev_unk = unk;
        }
    }
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-embed tokenizer::` — Expected: 4 passed.

- [ ] **Step 5: The lazy model slot (load, idle unload, background reload), tested without a model**

`crates/alice-embed/src/lazy.rs`:

```rust
//! A value that is loaded on demand, dropped after an idle time, and reloaded in the
//! background when a caller that must not wait asks for it.

use parking_lot::{Mutex, MutexGuard};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

type Loader<T> = Box<dyn Fn() -> alice_core::Result<T> + Send + Sync>;
/// Starts a background job (a named thread in production; tests inject failures).
type Spawner = Box<dyn Fn(Box<dyn FnOnce() + Send>) -> std::io::Result<()> + Send + Sync>;

pub(crate) struct Lazy<T: Send + 'static> {
    slot: Mutex<Option<T>>,
    loading: AtomicBool,
    started: Instant,
    last_used_ms: AtomicU64,
    idle: Option<Duration>,
    load: Loader<T>,
    spawn: Spawner,
    /// Why the last background load did not happen (cleared by the next successful load).
    last_error: Mutex<Option<String>>,
}

fn thread_spawner() -> Spawner {
    Box::new(|job| std::thread::Builder::new().name("alice-model-load".into()).spawn(job).map(|_| ()))
}

impl<T: Send + 'static> Lazy<T> {
    /// Loads now; with `idle` set, a reaper thread drops the value after that long unused.
    pub(crate) fn new(load: Loader<T>, idle: Option<Duration>) -> alice_core::Result<Arc<Self>> {
        Self::with_spawner(load, idle, thread_spawner())
    }

    pub(crate) fn with_spawner(load: Loader<T>, idle: Option<Duration>, spawn: Spawner) -> alice_core::Result<Arc<Self>> {
        let first = load()?;
        let me = Arc::new(Self {
            slot: Mutex::new(Some(first)),
            loading: AtomicBool::new(false),
            started: Instant::now(),
            last_used_ms: AtomicU64::new(0),
            idle,
            load,
            spawn,
            last_error: Mutex::new(None),
        });
        if let Some(d) = idle.filter(|d| !d.is_zero()) {
            let weak = Arc::downgrade(&me);
            std::thread::Builder::new()
                .name("alice-model-idle".into())
                .spawn(move || reaper(weak, d))
                .map_err(alice_core::AliceError::Io)?;
        }
        Ok(me)
    }

    fn touch(&self) {
        self.last_used_ms
            .store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    pub(crate) fn is_loaded(&self) -> bool {
        self.slot.lock().is_some()
    }

    /// Why the model is not loaded although a query asked for it, if a load failed.
    pub(crate) fn last_error(&self) -> Option<String> {
        self.last_error.lock().clone()
    }

    #[cfg(test)]
    fn unload(&self) {
        *self.slot.lock() = None;
    }

    /// Run `f` on the value, loading it first if needed (blocks). The lock is handed over
    /// fairly afterwards, so a waiting caller goes next.
    pub(crate) fn with_blocking<R>(&self, f: impl FnOnce(&mut T) -> R) -> alice_core::Result<R> {
        let mut guard = self.slot.lock();
        if guard.is_none() {
            *guard = Some((self.load)()?);
        }
        let r = f(guard.as_mut().expect("loaded above"));
        self.touch();
        MutexGuard::unlock_fair(guard);
        Ok(r)
    }

    /// Run `f` if the value is loaded; otherwise start loading it in the background and
    /// return `None` at once.
    pub(crate) fn with_loaded<R>(self: &Arc<Self>, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        let mut guard = self.slot.lock();
        if let Some(v) = guard.as_mut() {
            let r = f(v);
            self.touch();
            MutexGuard::unlock_fair(guard);
            return Some(r);
        }
        drop(guard);
        if !self.loading.swap(true, Ordering::AcqRel) {
            let me = self.clone();
            let job: Box<dyn FnOnce() + Send> = Box::new(move || {
                match (me.load)() {
                    Ok(v) => {
                        let mut g = me.slot.lock();
                        if g.is_none() {
                            *g = Some(v);
                        }
                        drop(g);
                        *me.last_error.lock() = None;
                        me.touch();
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "reloading the embedding model failed");
                        *me.last_error.lock() = Some(format!("reloading the embedding model failed: {e}"));
                    }
                }
                me.loading.store(false, Ordering::Release);
            });
            if let Err(e) = (self.spawn)(job) {
                // No thread, no load: say why, and let the next query try again.
                tracing::warn!(error = %e, "cannot start the model loader thread");
                *self.last_error.lock() = Some(format!("cannot start the model loader thread: {e}"));
                self.loading.store(false, Ordering::Release);
            }
        }
        None
    }

    /// Drop the value if it has not been used for `idle`; returns whether it was dropped.
    pub(crate) fn reap(&self) -> bool {
        let Some(idle) = self.idle else { return false };
        let now = self.started.elapsed().as_millis() as u64;
        let last = self.last_used_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) < idle.as_millis() as u64 {
            return false;
        }
        match self.slot.try_lock() {
            Some(mut g) if g.is_some() => {
                *g = None;
                tracing::info!(idle_secs = idle.as_secs(), "unloaded the embedding model after idle time");
                true
            }
            _ => false,
        }
    }
}

fn reaper<T: Send + 'static>(weak: Weak<Lazy<T>>, idle: Duration) {
    let tick = (idle / 4).clamp(Duration::from_millis(10), Duration::from_secs(30));
    loop {
        std::thread::sleep(tick);
        let Some(me) = weak.upgrade() else { return };
        me.reap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn counting(idle: Option<Duration>) -> (Arc<Lazy<usize>>, Arc<AtomicUsize>) {
        let loads = Arc::new(AtomicUsize::new(0));
        let l = loads.clone();
        let lazy = Lazy::new(
            Box::new(move || Ok(l.fetch_add(1, Ordering::SeqCst) + 1)),
            idle,
        )
        .unwrap();
        (lazy, loads)
    }

    #[test]
    fn without_idle_time_the_value_stays_loaded() {
        let (lazy, loads) = counting(None);
        assert_eq!(lazy.with_loaded(|v| *v), Some(1));
        assert!(!lazy.reap());
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn idle_value_is_dropped_then_reloaded_in_the_background_for_queries() {
        let (lazy, loads) = counting(Some(Duration::from_millis(60)));
        std::thread::sleep(Duration::from_millis(300));
        assert!(!lazy.is_loaded(), "the reaper dropped it");
        // A query does not wait: it gets None and triggers one background load.
        assert_eq!(lazy.with_loaded(|v| *v), None);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !lazy.is_loaded() {
            assert!(Instant::now() < deadline, "background reload");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(loads.load(Ordering::SeqCst), 2);
        // The worker path waits for a load instead.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(lazy.with_blocking(|v| *v).unwrap(), 3);
    }

    #[test]
    fn a_loader_thread_that_cannot_start_is_reported_and_retried_by_the_next_query() {
        let loads = Arc::new(AtomicUsize::new(0));
        let l = loads.clone();
        let fail_next = Arc::new(AtomicBool::new(true));
        let f = fail_next.clone();
        let lazy = Lazy::with_spawner(
            Box::new(move || Ok(l.fetch_add(1, Ordering::SeqCst) + 1)),
            None,
            // The first spawn fails; later ones run the job inline (deterministic).
            Box::new(move |job| {
                if f.swap(false, Ordering::SeqCst) {
                    return Err(std::io::Error::other("no threads left"));
                }
                job();
                Ok(())
            }),
        )
        .unwrap();
        lazy.unload();
        assert_eq!(lazy.with_loaded(|v| *v), None);
        assert!(lazy.last_error().unwrap().contains("cannot start the model loader thread"), "{:?}", lazy.last_error());
        assert_eq!(loads.load(Ordering::SeqCst), 1, "nothing loaded, and `loading` was reset");
        assert_eq!(lazy.with_loaded(|v| *v), None, "the next query starts the load again");
        assert_eq!(lazy.with_loaded(|v| *v), Some(2));
        assert!(lazy.last_error().is_none(), "a successful load clears the error");
    }
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-embed lazy::` — Expected: 3 passed.

- [ ] **Step 6: `OnnxEmbedder`**

`crates/alice-embed/src/onnx.rs`:

```rust
//! multilingual-e5-small (int8) on ONNX Runtime: the compact tokenizer, mean pooling over the
//! attention mask, L2 normalisation, `query: ` / `passage: ` prefixes. Same vectors as the
//! fastembed build used in M1 (`tests/onnx_equivalence.rs`), less memory: no tokenizer trie,
//! no CPU arena (ONNX Runtime would keep its largest call's working memory for good), no
//! memory patterns, the model read from its file by ONNX Runtime instead of a copy in memory.

use crate::lazy::Lazy;
use crate::tokenizer::UnigramTokenizer;
use crate::{DIM, MODEL_ID, is_model_present, model_dir};
use alice_core::embed::Embedder;
use alice_core::error::{AliceError, Result};
use ort::session::{Session, SessionInputValue};
use ort::value::Tensor;
use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Longest input the model reads, in tokens (including `<s>` and `</s>`).
const MAX_TOKENS: usize = 512;
/// Most texts per inference call.
const MAX_BATCH: usize = 32;
/// Padded tokens (texts × longest) per call: bounds working memory and how long a query
/// waits behind the worker (one call, ~115 ms for two 512-token texts on an M4 Pro).
const BATCH_TOKENS: usize = 1_024;

pub const MODEL_LOADING: &str =
    "the embedding model is loading after an idle period; this search used word matches only";

#[derive(Debug, Clone, Default)]
pub struct OnnxOptions {
    /// Unload the model after this long without use (`None` or zero: never).
    pub idle_unload: Option<Duration>,
    /// ONNX Runtime intra-op threads (`None`: all cores).
    pub intra_threads: Option<usize>,
}

struct Model {
    session: Session,
    token_types: bool,
}

pub struct OnnxEmbedder {
    tokenizer: UnigramTokenizer,
    model: Arc<Lazy<Model>>,
}

fn ort_err(e: impl std::fmt::Display) -> AliceError {
    AliceError::Other(format!(
        "the embedding model failed: {e}. Run \"alice model fetch\" to repair the model files"
    ))
}

fn load(onnx: &Path, threads: Option<usize>) -> Result<Model> {
    let threads = threads
        .or_else(|| std::thread::available_parallelism().ok().map(|n| n.get()))
        .unwrap_or(4);
    let session = Session::builder()
        .map_err(ort_err)?
        .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level3)
        .map_err(ort_err)?
        .with_intra_threads(threads)
        .map_err(ort_err)?
        .with_memory_pattern(false)
        .map_err(ort_err)?
        .with_execution_providers([ort::ep::CPU::default()
            .with_arena_allocator(false)
            .build()])
        .map_err(ort_err)?
        .commit_from_file(onnx)
        .map_err(ort_err)?;
    let token_types = session
        .inputs()
        .iter()
        .any(|i| i.name() == "token_type_ids");
    Ok(Model {
        session,
        token_types,
    })
}

/// Consecutive ranges of `lens` (token counts) with at most `max_batch` texts and at most
/// `budget` padded tokens each; a text longer than the budget gets a range of its own.
pub(crate) fn token_batches(lens: &[usize], max_batch: usize, budget: usize) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let (mut start, mut longest) = (0, 0);
    for (i, &len) in lens.iter().enumerate() {
        let n = i - start;
        if n > 0 && (n >= max_batch || (n + 1) * longest.max(len) > budget) {
            out.push(start..i);
            (start, longest) = (i, 0);
        }
        longest = longest.max(len);
    }
    if start < lens.len() {
        out.push(start..lens.len());
    }
    out
}

fn run(m: &mut Model, batch: &[Vec<u32>]) -> Result<Vec<Vec<f32>>> {
    let b = batch.len();
    let l = batch.iter().map(Vec::len).max().unwrap_or(0);
    let mut ids = vec![0i64; b * l];
    let mut mask = vec![0i64; b * l];
    for (i, row) in batch.iter().enumerate() {
        for (j, &t) in row.iter().enumerate() {
            ids[i * l + j] = i64::from(t);
            mask[i * l + j] = 1;
        }
    }
    let shape = [b as i64, l as i64];
    let mut inputs: Vec<(Cow<'static, str>, SessionInputValue<'static>)> = vec![
        ("input_ids".into(), Tensor::from_array((shape, ids)).map_err(ort_err)?.into()),
        ("attention_mask".into(), Tensor::from_array((shape, mask.clone())).map_err(ort_err)?.into()),
    ];
    if m.token_types {
        inputs.push((
            "token_type_ids".into(),
            Tensor::from_array((shape, vec![0i64; b * l])).map_err(ort_err)?.into(),
        ));
    }
    let outputs = m.session.run(inputs).map_err(ort_err)?;
    let hidden = match outputs.get("last_hidden_state") {
        Some(v) => v,
        None => &outputs[0],
    };
    let (out_shape, data) = hidden.try_extract_tensor::<f32>().map_err(ort_err)?;
    let h = out_shape[2] as usize;
    let mut vectors = Vec::with_capacity(b);
    for i in 0..b {
        let mut v = vec![0f32; h];
        let mut count = 0f32;
        for j in 0..l {
            if mask[i * l + j] == 1 {
                let row = &data[(i * l + j) * h..(i * l + j + 1) * h];
                v.iter_mut().zip(row).for_each(|(a, x)| *a += x);
                count += 1.0;
            }
        }
        v.iter_mut().for_each(|a| *a /= count.max(1.0));
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        v.iter_mut().for_each(|a| *a /= norm);
        vectors.push(v);
    }
    Ok(vectors)
}

impl OnnxEmbedder {
    pub fn new(models_dir: &Path, opts: OnnxOptions) -> Result<Self> {
        if !is_model_present(models_dir) {
            return Err(AliceError::Other(
                "the embedding model is not downloaded; run \"alice model fetch\"".into(),
            ));
        }
        let dir = model_dir(models_dir);
        let tokenizer = UnigramTokenizer::from_file(&dir.join("tokenizer.json"))?;
        let onnx = dir.join("model_quantized.onnx");
        let threads = opts.intra_threads;
        let model = Lazy::new(Box::new(move || load(&onnx, threads)), opts.idle_unload)?;
        Ok(Self { tokenizer, model })
    }

    fn encode_all(&self, prefix: &str, texts: &[String]) -> Vec<Vec<u32>> {
        texts
            .iter()
            .map(|t| self.tokenizer.encode(&format!("{prefix}{t}"), MAX_TOKENS))
            .collect()
    }
}

impl Embedder for OnnxEmbedder {
    fn model_id(&self) -> &str {
        MODEL_ID
    }
    fn dim(&self) -> usize {
        DIM
    }
    fn is_loaded(&self) -> bool {
        self.model.is_loaded()
    }
    fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let encoded = self.encode_all("passage: ", texts);
        let lens: Vec<usize> = encoded.iter().map(Vec::len).collect();
        let mut out = Vec::with_capacity(texts.len());
        for range in token_batches(&lens, MAX_BATCH, BATCH_TOKENS) {
            out.extend(self.model.with_blocking(|m| run(m, &encoded[range]))??);
        }
        Ok(out)
    }
    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let ids = self.encode_all("query: ", std::slice::from_ref(&text.to_string()));
        match self.model.with_loaded(|m| run(m, &ids)) {
            Some(r) => r?
                .pop()
                .ok_or_else(|| AliceError::Other("embedding returned nothing".into())),
            // A failed (re)load is named, so the notice is not "loading" forever; the next
            // query tries again.
            None => Err(AliceError::Other(match self.model.last_error() {
                Some(why) => format!("{MODEL_LOADING} ({why})"),
                None => MODEL_LOADING.into(),
            })),
        }
    }
}
```

In `crates/alice-core/src/embed.rs` add to the trait:

```rust
    /// False while a model that was unloaded after an idle period is being loaded again.
    fn is_loaded(&self) -> bool {
        true
    }
```

In `lib.rs`: remove the `#[cfg(feature = "local")] mod local { … }` block and its re-export; add

```rust
// The tokenizer needs `spm_precompiled`, an optional dependency of feature `local`.
#[cfg(feature = "local")]
pub mod tokenizer;
#[cfg(feature = "local")]
mod lazy;
#[cfg(feature = "local")]
pub mod onnx;
#[cfg(feature = "local")]
pub use onnx::{OnnxEmbedder, OnnxOptions};
```

Move the M1 test `passages_are_batched_by_padded_tokens_in_order` into `onnx.rs` tests unchanged except its `use` (M1 already batches by token counts: `use super::token_batches;`). Give `OnnxEmbedder` the two inspection methods M1's `FastEmbedder` had, with the same meaning, and use them in `embed_passages`:

```rust
impl OnnxEmbedder {
    /// Token count of each passage as the model sees it (prefix included, capped at 512).
    #[doc(hidden)]
    pub fn token_counts(&self, texts: &[String]) -> Vec<usize> {
        self.encode_all("passage: ", texts).iter().map(Vec::len).collect()
    }

    /// The batches `embed_passages` runs for `texts`, in order.
    pub fn passage_batches(&self, texts: &[String]) -> Vec<std::ops::Range<usize>> {
        token_batches(&self.token_counts(texts), MAX_BATCH, BATCH_TOKENS)
    }
}
```

so the ignored M1 test `tests/real_model.rs::passage_batches_respect_the_token_budget_for_dense_text` (it calls `e.token_counts`, `e.passage_batches`, `e.embed_passages`) compiles and keeps its meaning with only the constructor changed. The workspace must build after this task **including ignored tests** (`cargo test --workspace` compiles them): grep `crates/` for `FastEmbedder` and `alice_embed::local` and port every hit. Keep `loading_without_the_model_says_how_to_fetch_it`, now constructing `OnnxEmbedder::new(d.path(), OnnxOptions::default())`.

Callers: `crates/alice-daemon/src/daemon.rs`:

```rust
            Ok(Arc::new(alice_embed::OnnxEmbedder::new(
                &models,
                alice_embed::OnnxOptions {
                    // Task 6 reads this from `embedding.idle_unload`.
                    idle_unload: Some(Duration::from_secs(600)),
                    intra_threads: None,
                },
            )?))
```

`bench.rs` (both real constructions), `tests/real_model.rs`, `tests/real_search_quality.rs`, `tests/binary_chunk_recall.rs`: `alice_embed::OnnxEmbedder::new(&dir, alice_embed::OnnxOptions::default())`.

Run: `nice -n 15 cargo test -j 6 --workspace` — Expected: all pass (no test loads the real model).

- [ ] **Step 7: Equivalence with fastembed (real model, ignored)**

`crates/alice-embed/tests/onnx_equivalence.rs`:

```rust
//! The compact tokenizer and the direct ONNX Runtime session give the vectors of M1's
//! fastembed build: identical token ids on a multilingual corpus, cosine ≥ 0.9999 per vector.
//!   ALICE_MODELS_DIR=~/.alice/models cargo test -p alice-embed --release \
//!     --test onnx_equivalence -- --ignored --nocapture
#![cfg(feature = "local")]

use alice_core::embed::{Embedder, cosine};
use alice_embed::tokenizer::UnigramTokenizer;
use alice_embed::{OnnxEmbedder, OnnxOptions, model_dir};
use fastembed::{
    InitOptionsUserDefined, Pooling, QuantizationMode, TextEmbedding, TokenizerFiles,
    UserDefinedEmbeddingModel,
};
use std::path::{Path, PathBuf};

fn models_dir() -> PathBuf {
    std::env::var_os("ALICE_MODELS_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".alice/models")))
        .expect("set ALICE_MODELS_DIR")
}

fn fastembed(dir: &Path) -> TextEmbedding {
    let rd = |f: &str| std::fs::read(dir.join(f)).unwrap();
    let model = UserDefinedEmbeddingModel::new(
        rd("model_quantized.onnx"),
        TokenizerFiles {
            tokenizer_file: rd("tokenizer.json"),
            config_file: rd("config.json"),
            special_tokens_map_file: rd("special_tokens_map.json"),
            tokenizer_config_file: rd("tokenizer_config.json"),
        },
    )
    .with_pooling(Pooling::Mean)
    .with_quantization(QuantizationMode::Static);
    TextEmbedding::try_new_from_user_defined(model, InitOptionsUserDefined::new().with_max_length(512))
        .unwrap()
}

/// Edge cases first, then lines from the repo's docs and sources.
fn corpus() -> Vec<String> {
    let mut v: Vec<String> = [
        "hello world", "  leading and trailing  ", "tabs\tand\nnewlines\r\nhere",
        "ภาษาไทยไม่มีช่องว่างระหว่างคำ", "แก้บั๊ก writer lease หมดเวลา 120 วินาที",
        "HostLinkReconnect writer_lease internal/teacher/jev/client.go",
        "ｆｕｌｌｗｉｄｔｈ ｔｅｘｔ and ﬁ ligature", "emoji 🎉👍🏽 and CJK 漢字かな",
        "combining e\u{0301} and ำ vs \u{0E4D}\u{0E32}", "private use \u{E000}\u{E001} chars",
        "a ▁ literal metaspace", "UPPER lower MiXeD 12345 3.14 v2.0.1",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    v.push("long text ".repeat(400));
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for f in ["README.md", "README.th.md", "docs/superpowers/specs/2026-10-13-alice-core-design.md"] {
        for line in std::fs::read_to_string(root.join(f)).unwrap().lines() {
            let t = line.trim();
            if t.len() > 10 && !["<s>", "</s>", "<pad>", "<unk>", "<mask>"].iter().any(|s| t.contains(s)) {
                v.push(t.to_string());
            }
        }
    }
    v.truncate(2_000);
    v
}

#[test]
#[ignore = "needs the real model in ALICE_MODELS_DIR or ~/.alice/models"]
fn token_ids_match_hugging_face() {
    let dir = model_dir(&models_dir());
    let ours = UnigramTokenizer::from_file(&dir.join("tokenizer.json")).unwrap();
    let theirs = fastembed(&dir);
    let mut mismatches = Vec::new();
    for t in corpus() {
        for text in [format!("passage: {t}"), format!("query: {t}")] {
            let want: Vec<u32> = theirs.tokenizer.encode(text.as_str(), true).unwrap().get_ids().to_vec();
            let got = ours.encode(&text, 512);
            if got != want {
                mismatches.push(format!("{text:?}\n  ours   {got:?}\n  theirs {want:?}"));
            }
        }
    }
    assert!(mismatches.is_empty(), "{} mismatches:\n{}", mismatches.len(), mismatches.join("\n"));
}

#[test]
#[ignore = "needs the real model in ALICE_MODELS_DIR or ~/.alice/models"]
fn vectors_match_fastembed() {
    let models = models_dir();
    let ours = OnnxEmbedder::new(&models, OnnxOptions::default()).unwrap();
    let mut theirs = fastembed(&model_dir(&models));
    let texts: Vec<String> = corpus().into_iter().take(200).collect();
    let a = ours.embed_passages(&texts).unwrap();
    let prefixed: Vec<String> = texts.iter().map(|t| format!("passage: {t}")).collect();
    let b = theirs.embed(prefixed, Some(8)).unwrap();
    let worst = a.iter().zip(&b).map(|(x, y)| cosine(x, y)).fold(1.0f32, f32::min);
    println!("worst passage cosine {worst:.6}");
    assert!(worst >= 0.9999, "worst cosine {worst}");
    for t in texts.iter().take(20) {
        let q = ours.embed_query(t).unwrap();
        let r = theirs.embed(vec![format!("query: {t}")], None).unwrap().remove(0);
        assert!(cosine(&q, &r) >= 0.9999, "{t:?}");
    }
}
```

Run:

```bash
ALICE_MODELS_DIR=$HOME/.alice/models nice -n 15 cargo test -j 6 -p alice-embed --release \
  --test onnx_equivalence -- --ignored --nocapture
```

Expected: both pass. A token mismatch is a bug in `tokenizer.rs` (fix it; do not loosen the test). If only texts with characters the Precompiled map changes into several code points disagree, compare `ours.normalize(text)` with `theirs.tokenizer.get_normalizer()` output on that text to find the step that differs.

- [ ] **Step 8: Gold tests with the new embedder**

```bash
ALICE_MODELS_DIR=$HOME/.alice/models nice -n 15 cargo test -j 6 -p alice-embed --release \
  --test real_search_quality -- --ignored --nocapture
ALICE_MODELS_DIR=$HOME/.alice/models nice -n 15 cargo test -j 6 -p alice-embed --release \
  --test binary_chunk_recall -- --ignored --nocapture
```

Expected: unchanged results (MRR 1.000, 13/13 top 3, fallback 6/6; recall@10 as in Task 1).

- [ ] **Step 9: Measure the budget at 1M**

```bash
export S=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2 ALICE_HOME=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2/home; mkdir -p "$ALICE_HOME"; [ -e "$ALICE_HOME/models" ] || ln -s "$HOME/.alice/models" "$ALICE_HOME/models"   # scratch prelude (Global Constraints)
nice -n 15 cargo build --release -j 6 -p alice-memory && cp target/release/alice $S/alice-onnx
nice -n 15 $S/alice-onnx bench --real --prefill-vectors --chunks 1000000 --memories 10000 \
  --dir $S/data-1m-prefill --out $S/onnx-1m.json
pgrep -fl "alice daemon run" || true
```

Gate: `daemon.peak_rss_mb < 800` and, on macOS, `daemon.peak_footprint_mb < 800`; `latency.remember_p95_ms < 50`, `recall_p95_ms < 200`, `recall_raw_p95_ms < 200`, `context_p95_ms < 150`; `daemon.burst_errors == 0`.

**If the peak is still ≥ 800 MB**, apply these in order, re-running Step 9 after each and keeping a change only if it was needed (all latency gates must still hold): (1) `READER_CACHE_KIB` 16 MiB → 8 MiB in `alice-core/src/store/mod.rs`; (2) `WRITER_CACHE_KIB` 64 → 32 MiB; (3) `intra_threads: Some(4)` in the daemon's `OnnxOptions` (each ORT thread keeps scratch buffers). If all three are applied and the gate still fails, stop and report the measurements with a footprint breakdown (`vmmap --summary <pid>` on macOS, `/proc/<pid>/smaps_rollup` on Linux) instead of relaxing the budget.

(The idle-unload saving is measured in Task 6, once `embedding.idle_unload` is a setting.)

- [ ] **Step 10: Update `docs/benchmarks/m2.md`**

Add the column *ONNX Runtime + compact tokenizer* to the memory table (from `onnx-1m.json`), a "What each change saved" list (sign-bit chunks, tokenizer, arena off, any of the fallback steps used), and the verdict line "Daemon RSS < 800 MB at 1M chunks: met (peak N MB RSS, M MB footprint)". Add the equivalence results (0 token mismatches over N texts; worst cosine).

- [ ] **Step 11: Commit**

```bash
nice -n 15 cargo fmt --all && nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
git add Cargo.toml Cargo.lock crates/alice-embed crates/alice-core/src/embed.rs crates/alice-core/src/store/mod.rs \
  crates/alice-daemon/src/daemon.rs crates/alice/src/bench.rs docs/benchmarks/m2.md
git commit -m "perf: ONNX Runtime embedder with a compact Unigram tokenizer and idle unload; daemon under 800 MB at 1M chunks

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Windows build — usearch without NumKong, Unicode-safe index files, CI green on windows-latest

usearch 2.26.4's default `numkong` feature makes its C++ refer to the NumKong SIMD kernels through `__declspec(dllimport)` names that the static NumKong library does not provide under MSVC (`LNK2019: __imp_nk_capabilities` and four more), so nothing that links usearch builds on Windows. With `default-features = false`, usearch's `build.rs` defines `USEARCH_USE_NUMKONG=0` and compiles its own portable distance code; NumKong is not built at all. After Task 1 usearch only holds the memory vectors (thousands to a few hundred thousand), so the slower distance kernels cost little; this task measures it. usearch also opens files through C paths, which fails for non-ASCII Windows paths (a Thai user name in `ALICE_HOME`): index parts are now read and written by Rust (`load_from_buffer` / `save_to_buffer`).

**Files:**
- Modify: `Cargo.toml` (`usearch = { version = "2.26.4", default-features = false }`)
- Modify: `crates/alice-core/src/vector.rs` (`Part::open` / `Part::save` through buffers; drop `utf8()`; replace the `non_utf8_paths_are_invalid` test)
- Modify: `crates/alice-core/src/project.rs` (strip the `\\?\` verbatim prefix from canonical paths before `git -C` and key building, Windows only)
- Modify: `.github/workflows/ci.yml` (`--locked`; Windows runs the same steps)
- Modify: test files that fail on Windows (gate with a reason; see Step 6)
- Modify: `README.md`, `README.th.md` (platform table), `docs/benchmarks/m2.md` (Windows section)

**Interfaces:**
- Consumes: Task 1's `VectorIndex` (`Part` for memories, `ChunkPart` for chunks — `ChunkPart` already uses `std::fs`).
- Produces: `VectorIndex::open`/`save` accept any path the OS accepts (non-UTF-8, non-ASCII); `alice_core::project::strip_verbatim(p: PathBuf) -> PathBuf` (identity on non-Windows).

- [ ] **Step 1: Read the files this task modifies, as they are now.**

- [ ] **Step 2: Measure memory-search latency with NumKong (before)**

```bash
export S=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2 ALICE_HOME=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2/home; mkdir -p "$ALICE_HOME"; [ -e "$ALICE_HOME/models" ] || ln -s "$HOME/.alice/models" "$ALICE_HOME/models"   # scratch prelude (Global Constraints)
nice -n 15 cargo build --release -j 6 -p alice-memory && cp target/release/alice $S/alice-numkong
nice -n 15 $S/alice-numkong bench --chunks 100000 --memories 20000 --out $S/numkong-100k.json
```

Keep `latency.recall_p50_ms`, `recall_p95_ms`. (20,000 memories so the memory graph is not trivial.)

- [ ] **Step 3: Write the failing path test**

In `crates/alice-core/src/vector.rs` tests, replace `non_utf8_paths_are_invalid` with:

```rust
    /// usearch opens files through C paths; the parts are read and written by Rust instead,
    /// so a data directory with a Thai name (or, on Unix, non-UTF-8 bytes) works.
    #[test]
    fn index_files_save_and_open_under_unusual_directory_names() {
        let e = FakeEmbedder::new(384);
        let d = tempfile::tempdir().unwrap();
        let mut dirs = vec![d.path().join("ข้อมูล ทดสอบ").join("vectors")];
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            dirs.push(d.path().join(std::ffi::OsStr::from_bytes(b"bad\xffdir")));
        }
        for dir in dirs {
            std::fs::create_dir_all(&dir).unwrap();
            let base = dir.join("fake.usearch");
            let idx = VectorIndex::for_model("fake", 384).unwrap();
            idx.upsert(VecKey::Memory(1), &v(&e, "unicode path")).unwrap();
            idx.upsert(VecKey::Chunk(1), &v(&e, "chunk")).unwrap();
            idx.save(&base).unwrap();
            let back = VectorIndex::open(&base, "fake", 384).unwrap();
            assert_eq!(back.len(), 2, "{}", dir.display());
            assert_eq!(
                back.search(&v(&e, "unicode path"), 1, KeyFilter::Memories).unwrap()[0].0,
                VecKey::Memory(1)
            );
        }
    }
```

Run: `nice -n 15 cargo test -j 6 -p alice-core index_files_save_and_open` — Expected on macOS: FAIL for the non-UTF-8 directory ("not valid UTF-8").

- [ ] **Step 4: Read and write the memory part through buffers; drop NumKong**

Root `Cargo.toml`: `usearch = { version = "2.26.4", default-features = false }`.

In `vector.rs`, delete `fn utf8` and every call to it, and replace `Part::open` and the snapshot line of `Part::save`:

```rust
    /// Fails if the file is missing, unreadable or was built with another dimension, metric
    /// or quantization.
    fn open(path: &Path, dim: usize) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                AliceError::NotFound(format!("vector index {}", path.display()))
            }
            _ => AliceError::Io(e),
        })?;
        let index = Index::new(&options(dim)).map_err(err)?;
        index.load_from_buffer(&bytes).map_err(err)?;
        drop(bytes);
        if index.dimensions() != dim
            || index.metric_kind() != MetricKind::Cos
            || index.scalar_kind() != ScalarKind::I8
        {
            return Err(err(format!(
                "{} holds a {}-dimension {:?}/{:?} index; {dim}-dimension Cos/I8 expected",
                path.display(),
                index.dimensions(),
                index.metric_kind(),
                index.scalar_kind()
            )));
        }
        let cap = (index.size() * 2).max(MIN_CAPACITY);
        index.reserve_capacity_and_threads(cap, THREADS).map_err(err)?;
        let part = Self::wrap(index, THREADS);
        *part.synced.lock() = Some(path.to_path_buf());
        Ok(part)
    }
```

and in `Part::save`, instead of `self.inner.read().save(tmp_name)`:

```rust
        // The read guard lives for this block only: searches go on, writers wait above.
        let snapshot: Result<Vec<u8>> = {
            let i = self.inner.read();
            let mut buf = vec![0u8; i.serialized_length()];
            i.save_to_buffer(&mut buf).map_err(err).map(|()| buf)
        };
        let written = snapshot.and_then(|buf| {
            let mut f = std::fs::File::create(&tmp)?;
            std::io::Write::write_all(&mut f, &buf)?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)?;
            Ok(())
        });
```

(`tmp` is `path.with_extension("usearch.tmp")` as before.) Run Step 3's test and the whole crate: `nice -n 15 cargo test -j 6 -p alice-core` — Expected: pass.

- [ ] **Step 5: Measure again (after) and strip Windows verbatim prefixes**

```bash
export S=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2 ALICE_HOME=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2/home; mkdir -p "$ALICE_HOME"; [ -e "$ALICE_HOME/models" ] || ln -s "$HOME/.alice/models" "$ALICE_HOME/models"   # scratch prelude (Global Constraints)
nice -n 15 cargo build --release -j 6 -p alice-memory && cp target/release/alice $S/alice-portable
nice -n 15 $S/alice-portable bench --chunks 100000 --memories 20000 --out $S/portable-100k.json
```

Gate: `recall_p95_ms` stays < 200 ms (expected: within a few ms of Step 2). Record both in `docs/benchmarks/m2.md` under "Windows".

In `crates/alice-core/src/project.rs` add and use it on every `canonicalize()` result that is passed to `git -C` or turned into a `path:` key:

```rust
/// `canonicalize` on Windows returns `\\?\C:\…` (and `\\?\UNC\server\share\…`), which git and
/// people do not use; strip it so keys and `git -C` see the ordinary form. Identity elsewhere.
pub fn strip_verbatim(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest.to_string());
        }
    }
    p
}

#[cfg(test)]
#[test]
fn strip_verbatim_keeps_ordinary_paths() {
    let p = PathBuf::from("/tmp/x");
    assert_eq!(strip_verbatim(p.clone()), p);
    #[cfg(windows)]
    {
        assert_eq!(strip_verbatim(PathBuf::from(r"\\?\C:\work\repo")), PathBuf::from(r"C:\work\repo"));
        assert_eq!(strip_verbatim(PathBuf::from(r"\\?\UNC\srv\share\r")), PathBuf::from(r"\\srv\share\r"));
    }
}
```

- [ ] **Step 6: CI on Windows**

`.github/workflows/ci.yml`: add `--locked` to the clippy and test steps (Cargo.lock is committed). Commit what you have (Step 8's commit command, message `build: link on Windows (usearch without NumKong), Unicode-safe index files`) and push the branch:

```bash
git push -u origin feat/m2-judge-settings
gh run list --branch feat/m2-judge-settings --limit 1
gh run watch <run-id> --exit-status
```

Expected: macOS and Linux green as before; Windows now links. For each Windows failure, in this order of preference: (1) fix the code when the behaviour is wrong on Windows (paths, line endings, file locking that a test can avoid by closing handles before `TempDir` drops); (2) when the test checks Unix-only behaviour (file modes 0600/0700, signals, `libc::kill`, symlinks), gate it with `#[cfg(unix)]` on the test or the assertion block and a one-line comment saying why; (3) when it needs a real Windows machine to check (detached daemon start, process kill), `#[cfg_attr(windows, ignore = "<reason>")]`. Never delete a test. Push and re-run until all three OSes are green. List every gated test with its reason in `docs/benchmarks/m2.md` → "Windows".

- [ ] **Step 7: Platform table**

`README.md`: the Windows row becomes "Windows x86_64 — builds and passes the test suite in CI; local embeddings work (ONNX Runtime prebuilt). Auto-start of the daemon, `alice service install` (scheduled task) and process-level behaviour are not yet verified on a real Windows machine." Make the same change in `README.th.md` (Thai). Remove "to be fixed in M2" wording.

- [ ] **Step 8: Commit**

```bash
nice -n 15 cargo fmt --all && nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
git add Cargo.toml Cargo.lock .github/workflows/ci.yml crates docs/benchmarks/m2.md README.md README.th.md
git commit -m "build: link on Windows (usearch without NumKong), Unicode-safe index files, CI green on three OSes

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
git push
```

---

### Task 4: Schema v2, events with actors, history, undo, SSE stream

Spec §5.5: every mutation writes an `events` row with actor, action, target and before/after JSON; `alice undo <event-id>` reverses an event when its target has not changed since, otherwise it creates a review item. This task adds migration 2 (event and review indexes, `events.group_id`, `review_items.dedupe_key`), records events in the writer for every M1 mutation, implements undo over snapshots, the minimal review-item insert that undo needs (Task 11 builds the inbox on it), `alice history` / `alice undo`, the HTTP routes, and the SSE stream of spec §8.4 that pushes each recorded event.

**Files:**
- Create: `crates/alice-core/src/store/sql/0002_events_reviews.sql`
- Create: `crates/alice-core/src/store/events.rs`, `crates/alice-core/src/store/history.rs`, `crates/alice-core/src/store/reviews.rs`
- Modify: `crates/alice-core/src/store/mod.rs` (`pub mod events; pub mod history; pub mod reviews;`), `crates/alice-core/src/store/migrations.rs` (version 2)
- Modify: `crates/alice-core/src/api.rs` (`UpdateRequest.author`, `AssertFact.author`, `UndoRequest`, `UndoResponse`, `StreamItem`, `EventsRequest`, `MemoryFull.event_id`)
- Modify: `crates/alice-core/src/store/kg.rs` (`AssertFact.author`, `#[schemars(skip)]`)
- Modify: `crates/alice-daemon/src/writer.rs` (actors on ops, event recording, `WriteOp::Undo`, broadcast after commit), `service.rs`, `http.rs`, `client.rs`, `mcp.rs` (author on update/assert)
- Modify: `crates/alice/src/main.rs`, `crates/alice/src/cmd.rs` (`history`, `undo`)
- Test: unit tests in the new store modules, writer tests, `crates/alice-daemon/tests/http_api.rs`, `crates/alice/tests/cli.rs`

**Interfaces:**
- Consumes: `memories::{seq_of, get_by_seqs, apply_update, insert}`, `kg::assert_fact`, `normalize::content_hash`, `projects::get_by_id`.
- Produces:
  - `alice_core::store::events::{Actor, EventInput, EventRow, EventQuery, record, get, group, list, undone_by}`; `Actor::{Agent(String), User, Judge(String), Sync, Import, System}`, `Actor::from_author(&Author) -> Actor` (agent `None` or `"cli"` → `User`), serialised as `"agent:<name>"`, `"user"`, `"judge:<provider>"`, `"sync"`, `"import"`, `"system"`.
  - `alice_core::store::history::{MemorySnapshot, RelationSnapshot, snapshot_memory(conn, id) -> Result<Option<MemorySnapshot>>, snapshot_relation(conn, id) -> Result<Option<RelationSnapshot>>, undo(conn, event_id, actor, hlc: &mut dyn FnMut() -> String) -> Result<UndoOutcome>, UndoOutcome::{Undone { group: String, reverted: Vec<String> }, Conflict { review_id: String, reason: String }}}`
  - `alice_core::store::reviews::{ReviewType, ReviewStatus, NewReview, create(conn, &NewReview) -> Result<Option<String>>, count_open(conn) -> Result<u64>}`
  - Event actions: `memory.create`, `memory.edit`, `memory.supersede`, `memory.archive`, `memory.forget`, `memory.restore`, `relation.add`, `relation.close`, `undo` (marker, target `event:<id>`), and `undo:<original action>` for each reverted row. Later tasks add `judge.*`, `review.resolve`, `memory.merge`, `memory.purge`, `raw.retention`, `settings.change`, `secret.set`, `secret.delete`.
  - `WriteOp::{Update { id, update, actor }, AssertFact { fact, actor }, Undo { event_id, actor }}`; `WriteOutcome::Undo(UndoOutcome)`; `WriteOutcome::Updated(Memory, Option<String>)` (the update's event id, spec §8.1); `WriterHandle::subscribe() -> tokio::sync::broadcast::Receiver<StreamItem>`; `writer::record(conn, st, EventInput) -> Result<String>` (crate-internal helper later ops use; returns the event id).
  - `alice_core::api::{UndoRequest { event_id, author }, UndoResponse { undone: bool, reverted: Vec<String>, review_id: Option<String>, message: String }, StreamItem, EventsRequest = EventQuery}`; `MemoryFull::event_id: Option<String>` (set on `update` responses: the event to pass to `undo`; omitted from JSON when `None`).
  - `Service::{events(EventsRequest) -> Result<Vec<EventRow>>, undo(UndoRequest) -> Result<UndoResponse>, subscribe() -> broadcast::Receiver<StreamItem>, health_tick(&self) -> Duration}`; `ServiceOptions::health_tick: Duration` (default 5 s; tests use 100 ms).
  - HTTP: `GET /api/v1/events`, `POST /api/v1/undo`, `GET /api/v1/events/stream` (SSE, one `data:` JSON `StreamItem` per message, keep-alive every 15 s; a `StreamItem::Health` on connect and whenever health changes, checked every `health_tick`, ignoring `memory_bytes`). Client: `DaemonClient::{events, undo}`.
  - `tests/http_api.rs` helpers (extending M1's `Srv::start()`/`url()`/`post()`; later tasks reuse them by these names): fields `home: AliceHome`, `svc: Arc<Service>`; `Srv::start_with(ServiceOptions)`, `req(Method, path)`, `get`, `patch`, `put`, `delete` (each returns the `reqwest::Response`), `get_json::<R>(path)`, `post_json::<R>(path, body)` (assert 2xx, decode); free fns `ok_json(Response) -> R`, `next_item(&mut Response, &mut String) -> serde_json::Value` (the next SSE `data:` JSON within 5 s).
  - CLI: `alice history [<id>] [--actor A] [--limit N]`, `alice undo <event-id>`.

- [ ] **Step 1: Read the files this task modifies, as they are now.**

- [ ] **Step 2: Migration 2**

`crates/alice-core/src/store/sql/0002_events_reviews.sql`:

```sql
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
```

`migrations.rs`: `const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("sql/0001_init.sql")), (2, include_str!("sql/0002_events_reviews.sql"))]; pub const CURRENT_VERSION: i64 = 2;`. Add a test in `store/mod.rs` tests:

```rust
    #[test]
    fn a_v1_database_migrates_to_v2_after_a_backup() {
        let (d, path) = temp_db();
        {
            let mut conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)").unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(include_str!("sql/0001_init.sql")).unwrap();
            tx.execute("INSERT INTO schema_migrations VALUES (1, 'x')", []).unwrap();
            tx.commit().unwrap();
        }
        let conn = open_writer(&path).unwrap();
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('events')").unwrap()
            .query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
        assert!(cols.contains(&"group_id".to_string()));
        let backups = std::fs::read_dir(d.path()).unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains("pre-migration"))
            .count();
        assert_eq!(backups, 1, "a backup was taken before migrating");
    }
```

(Task 14 moves pre-migration backups into `backups/`; this test then asserts there.)

- [ ] **Step 3: Write the failing store tests**

`crates/alice-core/src/store/history.rs` test module (the module code comes in Step 5):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, Kind, Status};
    use crate::project::ProjectRef;
    use crate::store::events::{self, Actor, EventInput};
    use crate::store::memories::{self, MemoryUpdate, NewMemoryInput, UpdateAction};
    use crate::store::{open_writer, projects};

    struct T {
        _d: tempfile::TempDir,
        c: rusqlite::Connection,
        p: projects::ResolvedProject,
        n: u64,
    }

    impl T {
        fn new() -> T {
            let d = tempfile::tempdir().unwrap();
            let c = open_writer(&d.path().join("a.db")).unwrap();
            let p = projects::upsert_project(&c, &ProjectRef {
                key: "k".into(), slug: "app".into(), name: "app".into(), root: None, remote: None,
            }).unwrap();
            T { _d: d, c, p, n: 0 }
        }
        fn hlc(&mut self) -> String {
            self.n += 1;
            format!("{:013}-0000-dev", 1_000 + self.n)
        }
        /// remember + its event, as the writer does.
        fn remember(&mut self, text: &str) -> String {
            let h = self.hlc();
            let m = memories::prepare(NewMemoryInput { content: text.into(), kind: Some(Kind::Fact), ..Default::default() }, &self.p, h.clone()).unwrap();
            let memories::InsertOutcome::New { id, .. } = memories::insert(&self.c, &m).unwrap() else { panic!() };
            let after = snapshot_memory(&self.c, &id).unwrap().map(|s| serde_json::to_value(s).unwrap());
            events::record(&self.c, &EventInput {
                actor: &Actor::Agent("claude-code".into()), action: "memory.create",
                target_type: "memory", target_id: &id, before: None, after, group: None,
            }, &h).unwrap();
            id
        }
        fn edit(&mut self, id: &str, body: &str) -> String {
            let before = snapshot_memory(&self.c, id).unwrap().map(|s| serde_json::to_value(s).unwrap());
            let h = self.hlc();
            memories::apply_update(&self.c, id, &MemoryUpdate { action: UpdateAction::Edit, body: Some(body.into()), ..Default::default() }, h.clone()).unwrap();
            let after = snapshot_memory(&self.c, id).unwrap().map(|s| serde_json::to_value(s).unwrap());
            events::record(&self.c, &EventInput {
                actor: &Actor::User, action: "memory.edit", target_type: "memory",
                target_id: id, before, after, group: None,
            }, &h).unwrap()
        }
        fn body(&self, id: &str) -> String {
            self.c.query_row("SELECT body FROM memories WHERE id = ?1", [id], |r| r.get(0)).unwrap()
        }
        fn undo(&mut self, event: &str) -> Result<UndoOutcome> {
            let mut n = self.n;
            let mut tick = || { n += 1; format!("{:013}-0000-dev", 1_000 + n) };
            let out = undo(&self.c, event, &Actor::User, &mut tick);
            self.n = n;
            out
        }
    }

    #[test]
    fn undo_edit_restores_text_reindexes_and_records_the_undo() {
        let mut t = T::new();
        let id = t.remember("Use WAL mode for readers");
        let ev = t.edit(&id, "Use rollback journal");
        let out = t.undo(&ev).unwrap();
        assert!(matches!(out, UndoOutcome::Undone { ref reverted, .. } if reverted == &vec![ev.clone()]));
        assert_eq!(t.body(&id), "Use WAL mode for readers");
        let hit: i64 = t.c.query_row("SELECT count(*) FROM memories_fts WHERE memories_fts MATCH '\"WAL\"'", [], |r| r.get(0)).unwrap();
        assert_eq!(hit, 1, "FTS follows the restored text");
        let state: String = t.c.query_row("SELECT embed_state FROM memories WHERE id = ?1", [&id], |r| r.get(0)).unwrap();
        assert_eq!(state, "pending", "restored text is embedded again");
        assert_eq!(events::undone_by(&t.c, &ev).unwrap().is_some(), true);
    }

    #[test]
    fn undo_after_a_later_change_creates_a_review_item_and_changes_nothing() {
        let mut t = T::new();
        let id = t.remember("first text");
        let ev = t.edit(&id, "second text");
        t.edit(&id, "third text");
        match t.undo(&ev).unwrap() {
            UndoOutcome::Conflict { review_id, reason } => {
                assert!(reason.contains(&id), "{reason}");
                let kind: String = t.c.query_row("SELECT type FROM review_items WHERE id = ?1", [&review_id], |r| r.get(0)).unwrap();
                assert_eq!(kind, "undo");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(t.body(&id), "third text");
    }

    #[test]
    fn undo_twice_is_refused_and_undo_of_the_undo_redoes() {
        let mut t = T::new();
        let id = t.remember("a");
        let ev = t.edit(&id, "b");
        t.undo(&ev).unwrap();
        let err = t.undo(&ev).unwrap_err().to_string();
        assert!(err.contains("already undone"), "{err}");
        let marker = events::undone_by(&t.c, &ev).unwrap().unwrap();
        let redo_target = events::group(&t.c, marker.group_id.as_deref().unwrap()).unwrap()
            .into_iter().find(|e| e.action == "undo:memory.edit").unwrap();
        t.undo(&redo_target.id).unwrap();
        assert_eq!(t.body(&id), "b");
    }

    #[test]
    fn undo_of_a_create_trashes_and_a_purged_memory_cannot_come_back() {
        let mut t = T::new();
        let id = t.remember("temporary note");
        // Background activation (pending → active) does not count as a change since the event.
        let seq = memories::seq_of(&t.c, &id).unwrap().unwrap();
        memories::set_status_by_seq(&t.c, seq, Status::Active).unwrap();
        let create = events::list(&t.c, &events::EventQuery { target_id: Some(id.clone()), ..Default::default() }).unwrap().pop().unwrap();
        t.undo(&create.id).unwrap();
        let status: String = t.c.query_row("SELECT status FROM memories WHERE id = ?1", [&id], |r| r.get(0)).unwrap();
        assert_eq!(status, Status::Trashed.as_str());
        let id2 = t.remember("will be purged");
        let ev2 = t.edit(&id2, "edited");
        t.c.execute("DELETE FROM memories WHERE id = ?1", [&id2]).unwrap();
        let err = t.undo(&ev2).unwrap_err().to_string();
        assert!(err.contains("purged"), "{err}");
        let _ = Author::default();
    }
}
```

`crates/alice-core/src/store/events.rs` test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actors_round_trip_as_strings() {
        for a in [Actor::Agent("claude-code".into()), Actor::User, Actor::Judge("jev".into()), Actor::Sync, Actor::Import, Actor::System] {
            let s = a.as_string();
            assert_eq!(Actor::parse(&s), a, "{s}");
            assert_eq!(serde_json::to_value(&a).unwrap(), serde_json::Value::String(s));
        }
        use crate::model::Author;
        assert_eq!(Actor::from_author(&Author::default()), Actor::User);
        assert_eq!(Actor::from_author(&Author { agent: Some("cli".into()), ..Default::default() }), Actor::User);
        assert_eq!(Actor::from_author(&Author { agent: Some("codex".into()), ..Default::default() }), Actor::Agent("codex".into()));
    }

    #[test]
    fn list_is_newest_first_filters_and_pages() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let mut ids = Vec::new();
        for i in 0..5 {
            ids.push(record(&c, &EventInput {
                actor: &Actor::User, action: "memory.edit", target_type: "memory",
                target_id: if i % 2 == 0 { "A" } else { "B" },
                before: None, after: Some(serde_json::json!({"i": i})), group: None,
            }, &format!("{:013}-0000-d", i)).unwrap());
        }
        let all = list(&c, &EventQuery::default()).unwrap();
        assert_eq!(all.iter().map(|e| e.id.clone()).collect::<Vec<_>>(), ids.iter().rev().cloned().collect::<Vec<_>>());
        let a = list(&c, &EventQuery { target_id: Some("A".into()), ..Default::default() }).unwrap();
        assert_eq!(a.len(), 3);
        let page = list(&c, &EventQuery { before: Some(ids[3].clone()), limit: Some(2), ..Default::default() }).unwrap();
        assert_eq!(page.iter().map(|e| e.id.clone()).collect::<Vec<_>>(), vec![ids[2].clone(), ids[1].clone()]);
    }
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-core store::history store::events` — Expected: FAIL to compile.

- [ ] **Step 4: Implement `events.rs` and the minimal `reviews.rs`**

`crates/alice-core/src/store/events.rs`:

```rust
//! History: one row per change, with who made it and the target before and after (spec §5.5).

use crate::error::Result;
use crate::ids::{new_id, now_rfc3339};
use crate::model::Author;
use rusqlite::{Connection, OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Who made a change.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(into = "String", from = "String")]
#[schemars(with = "String")]
pub enum Actor {
    Agent(String),
    User,
    Judge(String),
    Sync,
    Import,
    /// Maintenance jobs (purges, retention).
    System,
}

impl Actor {
    pub fn as_string(&self) -> String {
        match self {
            Actor::Agent(a) => format!("agent:{a}"),
            Actor::User => "user".into(),
            Actor::Judge(p) => format!("judge:{p}"),
            Actor::Sync => "sync".into(),
            Actor::Import => "import".into(),
            Actor::System => "system".into(),
        }
    }

    pub fn parse(s: &str) -> Actor {
        match s {
            "user" => Actor::User,
            "sync" => Actor::Sync,
            "import" => Actor::Import,
            "system" => Actor::System,
            _ => match s.split_once(':') {
                Some(("agent", a)) => Actor::Agent(a.into()),
                Some(("judge", p)) => Actor::Judge(p.into()),
                _ => Actor::Agent(s.into()),
            },
        }
    }

    /// A write by an MCP client is the agent's; one from the CLI (`cli`) or the API is the user's.
    pub fn from_author(a: &Author) -> Actor {
        match a.agent.as_deref().map(str::trim) {
            None | Some("") | Some("cli") => Actor::User,
            Some(name) => Actor::Agent(name.into()),
        }
    }
}

impl From<Actor> for String {
    fn from(a: Actor) -> String {
        a.as_string()
    }
}

impl From<String> for Actor {
    fn from(s: String) -> Actor {
        Actor::parse(&s)
    }
}

pub struct EventInput<'a> {
    pub actor: &'a Actor,
    pub action: &'a str,
    pub target_type: &'a str,
    pub target_id: &'a str,
    pub before: Option<serde_json::Value>,
    pub after: Option<serde_json::Value>,
    pub group: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EventRow {
    pub id: String,
    pub ts: String,
    pub hlc: String,
    pub actor: String,
    pub action: String,
    pub target_type: String,
    pub target_id: String,
    pub before: Option<serde_json::Value>,
    pub after: Option<serde_json::Value>,
    pub group_id: Option<String>,
}

/// Events, newest first. `before` is an event id: only older events are returned (paging).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct EventQuery {
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub before: Option<String>,
    /// Default 50, at most 500.
    #[serde(default)]
    pub limit: Option<usize>,
}

const COLUMNS: &str =
    "id, ts, hlc, actor, action, target_type, target_id, before_json, after_json, group_id";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<EventRow> {
    let json = |i: usize| -> rusqlite::Result<Option<serde_json::Value>> {
        Ok(r.get::<_, Option<String>>(i)?
            .and_then(|s| serde_json::from_str(&s).ok()))
    };
    Ok(EventRow {
        id: r.get(0)?,
        ts: r.get(1)?,
        hlc: r.get(2)?,
        actor: r.get(3)?,
        action: r.get(4)?,
        target_type: r.get(5)?,
        target_id: r.get(6)?,
        before: json(7)?,
        after: json(8)?,
        group_id: r.get(9)?,
    })
}

pub fn record(conn: &Connection, e: &EventInput<'_>, hlc: &str) -> Result<String> {
    let id = new_id();
    conn.prepare_cached(&format!(
        "INSERT INTO events ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
    ))?
    .execute(params![
        id,
        now_rfc3339(),
        hlc,
        e.actor.as_string(),
        e.action,
        e.target_type,
        e.target_id,
        e.before.as_ref().map(|v| v.to_string()),
        e.after.as_ref().map(|v| v.to_string()),
        e.group,
    ])?;
    Ok(id)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<EventRow>> {
    Ok(conn
        .query_row(&format!("SELECT {COLUMNS} FROM events WHERE id = ?1"), [id], row)
        .optional()?)
}

/// Every event of a group, in the order they were recorded.
pub fn group(conn: &Connection, group_id: &str) -> Result<Vec<EventRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM events WHERE group_id = ?1 ORDER BY rowid"
    ))?;
    Ok(stmt.query_map([group_id], row)?.collect::<std::result::Result<_, _>>()?)
}

pub fn list(conn: &Connection, q: &EventQuery) -> Result<Vec<EventRow>> {
    let limit = q.limit.unwrap_or(50).clamp(1, 500) as i64;
    let before_rowid: Option<i64> = match &q.before {
        Some(id) => conn
            .query_row("SELECT rowid FROM events WHERE id = ?1", [id], |r| r.get(0))
            .optional()?,
        None => None,
    };
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM events
         WHERE (?1 IS NULL OR target_id = ?1) AND (?2 IS NULL OR actor = ?2)
           AND (?3 IS NULL OR rowid < ?3)
         ORDER BY rowid DESC LIMIT ?4"
    ))?;
    Ok(stmt
        .query_map(params![q.target_id, q.actor, before_rowid, limit], row)?
        .collect::<std::result::Result<_, _>>()?)
}

/// The `undo` marker event that reverted `event_id`, if any.
pub fn undone_by(conn: &Connection, event_id: &str) -> Result<Option<EventRow>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {COLUMNS} FROM events WHERE target_type = 'event' AND target_id = ?1 AND action = 'undo'
                 ORDER BY rowid DESC LIMIT 1"
            ),
            [event_id],
            row,
        )
        .optional()?)
}
```

`crates/alice-core/src/store/reviews.rs` (Task 11 adds listing and resolution):

```rust
//! Review inbox (spec §5.4): questions Alice could not settle on its own.

use crate::error::Result;
use crate::ids::{new_id, now_rfc3339};
use rusqlite::{Connection, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewType {
    Supersede,
    Contradiction,
    Merge,
    Predicate,
    Project,
    SyncConflict,
    /// An undo whose target changed since the event.
    Undo,
}

impl ReviewType {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewType::Supersede => "supersede",
            ReviewType::Contradiction => "contradiction",
            ReviewType::Merge => "merge",
            ReviewType::Predicate => "predicate",
            ReviewType::Project => "project",
            ReviewType::SyncConflict => "sync_conflict",
            ReviewType::Undo => "undo",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [Self::Supersede, Self::Contradiction, Self::Merge, Self::Predicate, Self::Project, Self::SyncConflict, Self::Undo]
            .into_iter()
            .find(|t| t.as_str() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Open,
    Resolved,
}

#[derive(Debug, Clone)]
pub struct NewReview {
    pub kind: ReviewType,
    pub payload: serde_json::Value,
    pub confidence: Option<f64>,
    /// Same key as an open item: no second item (the existing one already asks this).
    pub dedupe_key: Option<String>,
}

/// Insert an open item; `None` when an open item with the same `dedupe_key` exists.
pub fn create(conn: &Connection, r: &NewReview) -> Result<Option<String>> {
    let id = new_id();
    let n = conn.execute(
        "INSERT OR IGNORE INTO review_items (id, type, payload_json, confidence, status, created_at, dedupe_key)
         VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?6)",
        params![id, r.kind.as_str(), r.payload.to_string(), r.confidence, now_rfc3339(), r.dedupe_key],
    )?;
    Ok((n == 1).then_some(id))
}

pub fn count_open(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row("SELECT count(*) FROM review_items WHERE status = 'open'", [], |r| r.get(0))?;
    Ok(n as u64)
}
```

- [ ] **Step 5: Implement `history.rs` (snapshots and undo)**

```rust
//! Snapshots of rows that events record, and undo (spec §5.5): an event is reversed only if
//! its target still looks exactly as the event left it; otherwise a review item asks.

use crate::error::{AliceError, Result};
use crate::ids::{new_id, now_rfc3339};
use crate::model::{Kind, Status};
use crate::normalize::content_hash;
use crate::store::events::{self, Actor, EventInput, EventRow};
use crate::store::reviews::{self, NewReview, ReviewType};
use crate::store::{fts, memories};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// What undo restores for a memory: content, status, topics and outgoing links.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySnapshot {
    pub status: Status,
    pub kind: Kind,
    pub title: String,
    pub body: String,
    pub importance: u8,
    pub project_id: String,
    pub topics: Vec<String>,
    /// (to_id, link type), sorted.
    pub links_out: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelationSnapshot {
    pub subject_id: String,
    pub predicate: String,
    pub object_entity_id: Option<String>,
    pub object_literal: Option<String>,
    pub valid_from: String,
    pub valid_to: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoOutcome {
    Undone { group: String, reverted: Vec<String> },
    Conflict { review_id: String, reason: String },
}

pub fn snapshot_memory(conn: &Connection, id: &str) -> Result<Option<MemorySnapshot>> {
    let Some((status, kind, title, body, importance, project_id)) = conn
        .query_row(
            "SELECT status, kind, title, body, importance, project_id FROM memories WHERE id = ?1",
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let mut topics: Vec<String> = conn
        .prepare_cached("SELECT topic FROM memory_topics WHERE memory_id = ?1 ORDER BY topic")?
        .query_map([id], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    topics.sort();
    let links_out: Vec<(String, String)> = conn
        .prepare_cached("SELECT to_id, type FROM links WHERE from_id = ?1 ORDER BY to_id, type")?
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(Some(MemorySnapshot {
        status: Status::parse(&status).unwrap_or(Status::Active),
        kind: Kind::parse(&kind).unwrap_or(Kind::Note),
        title,
        body,
        importance,
        project_id,
        topics,
        links_out,
    }))
}

pub fn snapshot_relation(conn: &Connection, id: &str) -> Result<Option<RelationSnapshot>> {
    Ok(conn
        .query_row(
            "SELECT subject_id, predicate, object_entity_id, object_literal, valid_from, valid_to FROM relations WHERE id = ?1",
            [id],
            |r| Ok(RelationSnapshot {
                subject_id: r.get(0)?, predicate: r.get(1)?, object_entity_id: r.get(2)?,
                object_literal: r.get(3)?, valid_from: r.get(4)?, valid_to: r.get(5)?,
            }),
        )
        .optional()?)
}

fn current(conn: &Connection, target_type: &str, id: &str) -> Result<Option<serde_json::Value>> {
    Ok(match target_type {
        "memory" => snapshot_memory(conn, id)?.map(|s| serde_json::to_value(s).expect("snapshot")),
        "relation" => snapshot_relation(conn, id)?.map(|s| serde_json::to_value(s).expect("snapshot")),
        _ => None,
    })
}

/// Activation (`pending` → `active` once background work is done) is not a change anyone
/// made: compare the two as one state, so a memory can be undone after it was activated.
fn comparable(v: Option<serde_json::Value>) -> Option<serde_json::Value> {
    v.map(|mut v| {
        if v.get("status").and_then(|s| s.as_str()) == Some("pending") {
            v["status"] = serde_json::Value::String("active".into());
        }
        v
    })
}

fn restore_memory(conn: &Connection, id: &str, s: &MemorySnapshot, hlc: &str) -> Result<()> {
    let seq = memories::seq_of(conn, id)?.ok_or_else(|| AliceError::NotFound(format!("memory {id}")))?;
    let slug: String = conn.query_row("SELECT slug FROM projects WHERE id = ?1", [&s.project_id], |r| r.get(0))?;
    let hash = content_hash(&slug, s.kind, &s.title, &s.body);
    if s.status != Status::Trashed {
        let other: Option<String> = conn
            .query_row(
                "SELECT id FROM memories WHERE project_id = ?1 AND content_hash = ?2 AND status <> 'trashed' AND seq <> ?3",
                params![s.project_id, hash, seq],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(other) = other {
            return Err(AliceError::invalid(format!(
                "memory {other} now holds the text this undo would restore; forget or edit it first"
            )));
        }
    }
    let (old_title, old_body): (String, String) =
        conn.query_row("SELECT title, body FROM memories WHERE seq = ?1", [seq], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let text_changed = old_title != s.title || old_body != s.body;
    // A restored `pending` means "live": the row's own embed/judge states say what is left to do.
    let status = if s.status == Status::Pending { Status::Active } else { s.status };
    conn.execute(
        "UPDATE memories SET status = ?1, kind = ?2, title = ?3, body = ?4, importance = ?5, project_id = ?6,
            content_hash = ?7, updated_at = ?8, hlc = ?9,
            embed_state = CASE WHEN ?10 THEN 'pending' ELSE embed_state END
         WHERE seq = ?11",
        params![status.as_str(), s.kind.as_str(), s.title, s.body, s.importance, s.project_id, hash, now_rfc3339(), hlc, text_changed, seq],
    )?;
    conn.execute("DELETE FROM memory_topics WHERE memory_id = ?1", [id])?;
    for t in &s.topics {
        conn.execute("INSERT INTO memory_topics (memory_id, topic) VALUES (?1, ?2)", params![id, t])?;
    }
    fts::unindex_memory(conn, seq)?;
    fts::index_memory(conn, seq, &s.title, &s.body, &s.topics)?;
    conn.execute("DELETE FROM links WHERE from_id = ?1", [id])?;
    for (to, kind) in &s.links_out {
        conn.execute(
            "INSERT OR IGNORE INTO links (from_id, to_id, type, created_by, created_at) VALUES (?1, ?2, ?3, 'undo', ?4)",
            params![id, to, kind, now_rfc3339()],
        )?;
    }
    Ok(())
}

fn restore_relation(conn: &Connection, id: &str, s: Option<&RelationSnapshot>, hlc: &str) -> Result<()> {
    match s {
        None => {
            conn.execute("DELETE FROM relations WHERE id = ?1", [id])?;
        }
        Some(s) => {
            let n = conn.execute(
                "UPDATE relations SET valid_from = ?1, valid_to = ?2, hlc = ?3 WHERE id = ?4",
                params![s.valid_from, s.valid_to, hlc, id],
            )?;
            if n == 0 {
                conn.execute(
                    "INSERT INTO relations (id, subject_id, predicate, object_entity_id, object_literal, valid_from, valid_to, created_at, hlc)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![id, s.subject_id, s.predicate, s.object_entity_id, s.object_literal, s.valid_from, s.valid_to, now_rfc3339(), hlc],
                )?;
            }
        }
    }
    Ok(())
}

/// Never undone: markers and changes whose previous state no longer exists anywhere.
fn undoable(action: &str) -> bool {
    !matches!(action, "undo" | "memory.purge" | "raw.retention" | "settings.change" | "secret.set" | "secret.delete")
}

/// Reverse `event_id` (and every event of its group, newest first). Run in a transaction (the
/// writer does): either all of them are reverted or none.
pub fn undo(
    conn: &Connection,
    event_id: &str,
    actor: &Actor,
    hlc: &mut dyn FnMut() -> String,
) -> Result<UndoOutcome> {
    let ev = events::get(conn, event_id)?.ok_or_else(|| {
        AliceError::NotFound(format!("event {event_id}; list events with \"alice history\""))
    })?;
    if !undoable(&ev.action) {
        return Err(AliceError::invalid(format!(
            "event {event_id} ({}) cannot be undone", ev.action
        )));
    }
    if let Some(by) = events::undone_by(conn, event_id)? {
        return Err(AliceError::invalid(format!(
            "event {event_id} was already undone by event {}; undo that one to redo the change", by.id
        )));
    }
    let mut todo: Vec<EventRow> = match &ev.group_id {
        Some(g) => events::group(conn, g)?
            .into_iter()
            .filter(|e| undoable(&e.action))
            .collect(),
        None => vec![ev.clone()],
    };
    todo.reverse();
    for e in &todo {
        let now = current(conn, &e.target_type, &e.target_id)?;
        if now.is_none() && e.after.is_some() {
            return Err(AliceError::invalid(format!(
                "{} {} was purged from the trash; it cannot be restored", e.target_type, e.target_id
            )));
        }
        if comparable(now) != comparable(e.after.clone()) {
            let reason = format!("{} {} changed after event {}", e.target_type, e.target_id, e.id);
            let review_id = reviews::create(conn, &NewReview {
                kind: ReviewType::Undo,
                payload: serde_json::json!({"event_id": event_id, "target_type": e.target_type, "target_id": e.target_id, "reason": reason}),
                confidence: None,
                dedupe_key: Some(format!("undo:{event_id}")),
            })?
            .unwrap_or_else(|| "(already open)".into());
            return Ok(UndoOutcome::Conflict { review_id, reason });
        }
    }
    let group = new_id();
    let mut reverted = Vec::new();
    for e in &todo {
        let h = hlc();
        let before_now = current(conn, &e.target_type, &e.target_id)?;
        match (e.target_type.as_str(), &e.before) {
            ("memory", Some(b)) => {
                let s: MemorySnapshot = serde_json::from_value(b.clone())
                    .map_err(|err| AliceError::Other(format!("event {}: {err}", e.id)))?;
                restore_memory(conn, &e.target_id, &s, &h)?;
            }
            ("memory", None) => {
                // Undoing a create forgets the memory (it stays in the trash for 30 days).
                let mut s: MemorySnapshot = serde_json::from_value(e.after.clone().unwrap_or_default())
                    .map_err(|err| AliceError::Other(format!("event {}: {err}", e.id)))?;
                s.status = Status::Trashed;
                restore_memory(conn, &e.target_id, &s, &h)?;
            }
            ("relation", b) => {
                let s: Option<RelationSnapshot> = b.clone().map(serde_json::from_value).transpose()
                    .map_err(|err| AliceError::Other(format!("event {}: {err}", e.id)))?;
                restore_relation(conn, &e.target_id, s.as_ref(), &h)?;
            }
            (other, _) => {
                return Err(AliceError::invalid(format!("events on {other} cannot be undone")));
            }
        }
        let after_now = current(conn, &e.target_type, &e.target_id)?;
        events::record(conn, &EventInput {
            actor, action: &format!("undo:{}", e.action), target_type: &e.target_type,
            target_id: &e.target_id, before: before_now, after: after_now, group: Some(&group),
        }, &h)?;
        events::record(conn, &EventInput {
            actor, action: "undo", target_type: "event", target_id: &e.id,
            before: None, after: None, group: Some(&group),
        }, &h)?;
        reverted.push(e.id.clone());
    }
    Ok(UndoOutcome::Undone { group, reverted })
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-core store::` — Expected: the new tests pass with the old ones.

- [ ] **Step 6: API types**

In `crates/alice-core/src/api.rs` add:

```rust
pub use crate::store::events::{EventQuery as EventsRequest, EventRow};

/// Reverse a change: the event id comes from `alice history` or `/api/v1/events`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct UndoRequest {
    pub event_id: String,
    #[serde(default)]
    #[schemars(skip)]
    pub author: Author,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UndoResponse {
    pub undone: bool,
    /// Events reverted (the whole group of the one asked for).
    pub reverted: Vec<String>,
    /// Set when the target changed since: a review item asks what to do.
    pub review_id: Option<String>,
    pub message: String,
}

/// What `/api/v1/events/stream` pushes (one JSON object per SSE message).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamItem {
    Event { event: EventRow },
    Review { id: String, review_type: String },
    Settings { changed: Vec<String> },
    Health { health: Health },
}
```

and `#[serde(default)] #[schemars(skip)] pub author: Author` on `UpdateRequest` (and on `AssertFact` in `store/kg.rs`, with `use crate::model::Author`). Extend `schemas_hide_daemon_filled_fields` to check `UpdateRequest` and `AssertFactRequest` do not expose `author`.

- [ ] **Step 7: Record events in the writer; `WriteOp::Undo`; broadcast**

In `crates/alice-daemon/src/writer.rs`:

```rust
    Update {
        id: String,
        update: MemoryUpdate,
        actor: Actor,
    },
    AssertFact {
        fact: AssertFact,
        actor: Actor,
    },
    /// Reverse an event (and its group); a changed target yields a review item instead.
    Undo {
        event_id: String,
        actor: Actor,
    },
```

`WriteOutcome::Undo(UndoOutcome)`. In `State` add `stream: Vec<StreamItem>` (items this batch will broadcast after it commits; later tasks push reviews and settings changes here too) and give `spawn_writer` a `tokio::sync::broadcast::channel::<StreamItem>(256)` whose sender lives in the thread and whose `Sender` clone is kept in `WriterHandle` (field `stream`) for `subscribe()`:

```rust
impl WriterHandle {
    /// Changes committed from now on (events, reviews); slow receivers miss old items.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<StreamItem> {
        self.stream.subscribe()
    }
}

/// Record an event in the current transaction; it is broadcast after the batch commits.
/// Returns the event id.
pub(crate) fn record(conn: &Connection, st: &mut State, e: EventInput<'_>) -> Result<String> {
    let hlc = st.hlc.tick();
    let id = events::record(conn, &e, &hlc)?;
    if let Some(event) = events::get(conn, &id)? {
        st.stream.push(StreamItem::Event { event });
    }
    Ok(id)
}

fn snap(conn: &Connection, id: &str) -> Result<Option<serde_json::Value>> {
    Ok(history::snapshot_memory(conn, id)?.map(|s| serde_json::to_value(s).expect("snapshot")))
}
```

In `apply`:
- `Remember`: on `InsertOutcome::New { id, .. }`, with `actor = Actor::from_author(&prepared.author)` and a `group = new_id()` when `supersedes` is non-empty: record `memory.create` (before `None`, after `snap(id)`), and for each superseded id that changed status record `memory.supersede` with the snapshot taken before `insert` and after. (Take the before-snapshots of the `supersedes` ids before calling `memories::insert`.)
- `Update { id, update, actor }`: `before = snap(&id)`; for `UpdateAction::Supersede` also `before_by = snap(superseded_by)`; apply; record `memory.<action>` (`edit`, `supersede`, `archive`, `forget`, `restore`) for `id`, plus `memory.link` for `superseded_by` (its outgoing `supersedes` link), both in one group. Return `WriteOutcome::Updated(memory, Some(<the id `record` returned for `memory.<action>`>))` (M1's `Updated(Memory)` gains the event id; `None` only where no event was recorded).
- `AssertFact { fact, actor }`: before applying, if `kg::find_entity(conn, &fact.subject)` and the canonical predicate exist, collect the ids and snapshots of that subject's open relations for that predicate; apply; then for each collected id whose snapshot changed record `relation.close`, and for `outcome.relation_id` when it is new record `relation.add` (before `None`) — all in one group.
- `Undo { event_id, actor }`: `let mut tick = || st.hlc.tick();` → `history::undo(conn, &event_id, &actor, &mut tick)`; when `Undone`, push the group's rows (`events::group(conn, &group)`) onto `st.stream` as `StreamItem::Event`. Return `WriteOutcome::Undo(out)`.

After a successful commit in the writer loop, send every item of `st.stream` (ignore send errors: no subscribers), then clear it; on a failed commit clear it without sending. (Jobs rolled back by their savepoint must not leave items behind: remember `st.stream.len()` before each job and truncate back to it when the job fails.) Update every `WriteOp::Update`/`AssertFact` construction in tests to the struct forms with `actor: Actor::User`.

Writer tests to add (in `writer.rs` tests, using the existing fixture helpers):

```rust
    #[test]
    fn writes_record_events_with_their_actor_and_are_broadcast_after_commit() {
        let f = fixture();
        let mut rx = f.h.subscribe();
        let id = match f.h.submit_blocking(WriteOp::Remember {
            input: NewMemoryInput {
                content: "event me".into(),
                author: alice_core::model::Author { agent: Some("claude-code".into()), ..Default::default() },
                ..Default::default()
            },
            project: ProjectChoice::Slug("global".into()),
        }).unwrap() {
            WriteOutcome::Remembered(InsertOutcome::New { id, .. }, _) => id,
            o => panic!("{o:?}"),
        };
        let actor: String = f.read("SELECT actor FROM events WHERE target_id = ?1 AND action = 'memory.create'", [&id]);
        assert_eq!(actor, "agent:claude-code");
        match rx.try_recv().unwrap() {
            StreamItem::Event { event } => assert_eq!(event.target_id, id),
            other => panic!("{other:?}"),
        }
        f.h.submit_blocking(WriteOp::Update {
            id: id.clone(),
            update: MemoryUpdate { action: UpdateAction::Archive, ..Default::default() },
            actor: Actor::User,
        }).unwrap();
        let n: i64 = f.read("SELECT count(*) FROM events WHERE target_id = ?1 AND action = 'memory.archive' AND actor = 'user'", [&id]);
        assert_eq!(n, 1);
    }

    #[test]
    fn a_fact_supersede_records_the_closed_and_the_new_relation_in_one_group() {
        let f = fixture();
        for (object, action) in [("SQLite", FactAction::Add), ("Postgres", FactAction::Supersede)] {
            f.h.submit_blocking(WriteOp::AssertFact {
                fact: AssertFact { subject: "alice".into(), predicate: "uses".into(), object: object.into(), action, ..Default::default() },
                actor: Actor::User,
            }).unwrap();
        }
        let groups: i64 = f.read("SELECT count(DISTINCT group_id) FROM events WHERE action IN ('relation.close','relation.add') AND group_id IS NOT NULL", []);
        let closes: i64 = f.read("SELECT count(*) FROM events WHERE action = 'relation.close'", []);
        assert_eq!((groups, closes), (2, 1));
    }
```

Run: `nice -n 15 cargo test -j 6 -p alice-daemon writer::` — Expected: pass (after fixing the struct-variant call sites).

- [ ] **Step 8: Service, HTTP, client, MCP, CLI**

`service.rs`:

```rust
    pub async fn events(&self, req: EventsRequest) -> Result<Vec<EventRow>> {
        self.read(move |c| alice_core::store::events::list(c, &req)).await
    }

    pub async fn undo(&self, req: UndoRequest) -> Result<UndoResponse> {
        let actor = Actor::from_author(&req.author);
        match self.writer.submit(WriteOp::Undo { event_id: req.event_id.clone(), actor }).await? {
            WriteOutcome::Undo(UndoOutcome::Undone { reverted, .. }) => Ok(UndoResponse {
                undone: true,
                message: format!("Undone: {} change(s) reverted.", reverted.len()),
                reverted,
                review_id: None,
            }),
            WriteOutcome::Undo(UndoOutcome::Conflict { review_id, reason }) => Ok(UndoResponse {
                undone: false,
                reverted: Vec::new(),
                message: format!("Not undone: {reason}. Review item {review_id} asks what to do (alice review)."),
                review_id: Some(review_id),
            }),
            other => Err(unexpected(other)),
        }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<StreamItem> {
        self.writer.subscribe()
    }

    /// How often the SSE stream checks whether health changed.
    pub fn health_tick(&self) -> std::time::Duration {
        self.health_tick
    }

    pub async fn update(&self, req: UpdateRequest) -> Result<MemoryFull> {
        let actor = Actor::from_author(&req.author);
        let (m, event_id) = match self.writer.submit(WriteOp::Update { id: req.id, update: req.update, actor }).await? {
            WriteOutcome::Updated(m, event_id) => (m, event_id),
            other => return Err(unexpected(other)),
        };
        let mut out = self.read(move |c| full(c, m, false)).await?;
        out.event_id = event_id;
        Ok(out)
    }
```

`ServiceOptions` gains `pub health_tick: Duration` (`Default`: 5 s); `Service` keeps it in a `health_tick` field. `full(..)` builds `MemoryFull { .., event_id: None }`. `assert_fact` passes `actor: Actor::from_author(&req.author)`. The CLI command that calls `update` (M1 `cmd.rs` prints `"{id} is now {status}"`) appends ` (event {e}; undo with \`alice undo {e}\`)` when `r.event_id` is `Some(e)`; MCP `update_memory` already returns the `MemoryFull` JSON, which now carries `event_id`. `mcp.rs`: in `update_memory` and `assert_fact` tools, take `ctx: RequestContext<RoleServer>` and set `req.author.agent = Self::agent(&ctx)` when empty (as `remember` does).

`http.rs` routes (inside the guarded router):

```rust
        .route("/api/v1/events", get(events))
        .route("/api/v1/undo", post(undo))
        .route("/api/v1/events/stream", get(stream))
```

```rust
async fn events(
    State(st): State<AppState>,
    query: Result<Query<EventsRequest>, QueryRejection>,
) -> ApiResult<Vec<EventRow>> {
    let Query(q) = query.map_err(|r| ApiError(AliceError::Invalid(format!(
        "{}; use /api/v1/events?target_id=<id>&actor=<actor>&before=<event id>&limit=<1-500>", r.body_text()
    ))))?;
    Ok(Json(st.svc.events(q).await?))
}
async fn undo(State(st): State<AppState>, ApiJson(req): ApiJson<UndoRequest>) -> ApiResult<UndoResponse> {
    Ok(Json(st.svc.undo(req).await?))
}
/// Server-sent events: every committed change as JSON `StreamItem`s, plus the health on
/// connect and whenever it changes (checked every `health_tick`).
async fn stream(State(st): State<AppState>) -> axum::response::sse::Sse<impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use tokio::sync::broadcast::error::RecvError;
    struct Feed {
        rx: tokio::sync::broadcast::Receiver<StreamItem>,
        svc: Arc<Service>,
        tick: tokio::time::Interval,
        last: Option<serde_json::Value>,
    }
    fn event(item: &StreamItem) -> Event {
        Event::default().data(serde_json::to_string(item).unwrap_or_default())
    }
    let mut tick = tokio::time::interval(st.svc.health_tick());
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires at once: the stream opens with the current health.
    let feed = Feed { rx: st.svc.subscribe(), svc: st.svc.clone(), tick, last: None };
    let s = futures::stream::unfold(feed, |mut f| async move {
        loop {
            tokio::select! {
                item = f.rx.recv() => match item {
                    Ok(item) => return Some((Ok(event(&item)), f)),
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return None,
                },
                _ = f.tick.tick() => {
                    let Ok(health) = f.svc.health().await else { continue };
                    // Memory use changes on every reading; it alone is not a change.
                    let mut key = serde_json::to_value(&health).unwrap_or_default();
                    if let Some(o) = key.as_object_mut() { o.remove("memory_bytes"); }
                    if f.last.as_ref() != Some(&key) {
                        f.last = Some(key);
                        return Some((Ok(event(&StreamItem::Health { health })), f));
                    }
                }
            }
        }
    });
    Sse::new(s).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
}
```

`client.rs`:

```rust
    pub async fn events(&self, q: &EventsRequest) -> Result<Vec<EventRow>> {
        let mut url = String::from("/api/v1/events?");
        if let Some(t) = &q.target_id { url.push_str(&format!("target_id={}&", url_escape(t))); }
        if let Some(a) = &q.actor { url.push_str(&format!("actor={}&", url_escape(a))); }
        if let Some(b) = &q.before { url.push_str(&format!("before={}&", url_escape(b))); }
        if let Some(l) = q.limit { url.push_str(&format!("limit={l}")); }
        self.get_json(&url).await
    }
    pub async fn undo(&self, r: &UndoRequest) -> Result<UndoResponse> {
        self.call("/api/v1/undo", r, Duration::from_secs(10)).await
    }
```

(`url_escape` = `url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>()`.)

CLI (`main.rs` `Command`):

```rust
    /// Recent changes (all, or to one memory or fact): who changed what, with event ids for undo.
    History(cmd::HistoryArgs),
    /// Reverse a change by its event id (from `alice history`).
    Undo {
        event_id: String,
        /// Who undoes it (default: $ALICE_AGENT or "cli", as for `remember`).
        #[arg(long, env = "ALICE_AGENT", default_value = "cli")]
        agent: String,
    },
```

`cmd.rs`:

```rust
#[derive(Args)]
pub struct HistoryArgs {
    /// A memory or relation id; omit for all recent changes.
    pub target: Option<String>,
    #[arg(long)]
    pub actor: Option<String>,
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
}
```

```rust
        Command::History(a) => {
            let r = client(&home).await?.events(&EventsRequest {
                target_id: a.target, actor: a.actor, before: None, limit: Some(a.limit),
            }).await?;
            print(json, &r, || {
                r.iter().map(|e| format!("{}  {}  {:<16} {:<18} {} {}\n",
                    e.id, &e.ts[..19.min(e.ts.len())], e.actor, e.action, e.target_type, e.target_id)).collect()
            })
        }
        Command::Undo { event_id, agent } => {
            // `ALICE_AGENT=` (set but empty) means the default, as in `remember`.
            let agent = match agent.trim() { "" => "cli".to_string(), a => a.to_string() };
            let r = client(&home).await?.undo(&UndoRequest { event_id, author: Author { agent: Some(agent), ..Default::default() } }).await?;
            print(json, &r, || format!("{}\n", r.message))
        }
```

(The `agent` argument follows `RememberArgs.agent`: `$ALICE_AGENT` or `"cli"`; `cli` maps to actor `user`.)

- [ ] **Step 9: Interface tests**

`crates/alice-daemon/tests/http_api.rs`. First extend M1's `Srv` (this is the one definition of the helpers; Tasks 6, 11 and 17 use them by these names and add none of their own). Keep `start()`, `url()` and `post()` behaving as in M1 so the existing tests are unchanged:

```rust
struct Srv {
    _d: tempfile::TempDir,
    #[allow(dead_code)] // read by later tasks' tests
    home: AliceHome,
    #[allow(dead_code)]
    svc: Arc<Service>,
    port: u16,
    token: String,
    shutdown: CancellationToken,
    http: reqwest::Client,
}

#[allow(dead_code)] // not every helper is used by every task's tests
impl Srv {
    async fn start() -> Srv {
        Self::start_with(ServiceOptions::default()).await
    }
    /// `start()` with chosen service options (settings, transports, a short health tick).
    async fn start_with(opts: ServiceOptions) -> Srv {
        alice_daemon::install_tls();
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let svc = Service::start(
            home.clone(),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            opts,
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = "t".repeat(64);
        let shutdown = CancellationToken::new();
        let app = router(svc.clone(), token.clone(), port, shutdown.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Srv { _d: d, home, svc, port, token, shutdown, http: reqwest::Client::new() }
    }
    fn url(&self, p: &str) -> String {
        format!("http://127.0.0.1:{}{p}", self.port)
    }
    /// An authorised request with any method; the helpers below are shorthands.
    fn req(&self, method: reqwest::Method, p: &str) -> reqwest::RequestBuilder {
        self.http.request(method, self.url(p)).bearer_auth(&self.token)
    }
    async fn post<T: serde::Serialize>(&self, p: &str, body: &T) -> reqwest::Response {
        self.req(reqwest::Method::POST, p).json(body).send().await.unwrap()
    }
    async fn get(&self, p: &str) -> reqwest::Response {
        self.req(reqwest::Method::GET, p).send().await.unwrap()
    }
    async fn patch<T: serde::Serialize>(&self, p: &str, body: &T) -> reqwest::Response {
        self.req(reqwest::Method::PATCH, p).json(body).send().await.unwrap()
    }
    async fn put<T: serde::Serialize>(&self, p: &str, body: &T) -> reqwest::Response {
        self.req(reqwest::Method::PUT, p).json(body).send().await.unwrap()
    }
    async fn delete(&self, p: &str) -> reqwest::Response {
        self.req(reqwest::Method::DELETE, p).send().await.unwrap()
    }
    /// GET that must succeed, decoded.
    async fn get_json<R: serde::de::DeserializeOwned>(&self, p: &str) -> R {
        ok_json(self.get(p).await).await
    }
    /// POST that must succeed, decoded.
    async fn post_json<R: serde::de::DeserializeOwned, T: serde::Serialize>(&self, p: &str, body: &T) -> R {
        ok_json(self.post(p, body).await).await
    }
}

/// The body of a 2xx response, decoded; any other status fails the test, showing the body.
async fn ok_json<R: serde::de::DeserializeOwned>(r: reqwest::Response) -> R {
    let status = r.status();
    let text = r.text().await.unwrap();
    assert!(status.is_success(), "{status}: {text}");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

/// The next SSE message's JSON (its `data:` line) within 5 s; keep-alive comments are skipped.
async fn next_item(stream: &mut reqwest::Response, buf: &mut String) -> serde_json::Value {
    loop {
        if let Some(end) = buf.find("\n\n") {
            let msg: String = buf.drain(..end + 2).collect();
            if let Some(data) = msg.lines().find_map(|l| l.strip_prefix("data:")) {
                return serde_json::from_str(data.trim()).unwrap();
            }
            continue;
        }
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), stream.chunk())
            .await
            .expect("an SSE message within 5 s")
            .unwrap()
            .expect("the stream is open");
        buf.push_str(&String::from_utf8_lossy(&chunk));
    }
}
```

Then the tests:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn events_list_undo_and_stream() {
    let s = Srv::start().await;
    let mut stream = s.get("/api/v1/events/stream").await;
    assert_eq!(stream.status(), 200);
    let mut buf = String::new();
    assert_eq!(next_item(&mut stream, &mut buf).await["type"], "health", "the stream opens with the health");
    let r: RememberResponse = s.post_json("/api/v1/remember", &serde_json::json!({"content": "stream me"})).await;
    let id = r.id.unwrap();
    let item = loop {
        let v = next_item(&mut stream, &mut buf).await;
        if v["type"] == "event" { break v; }
    };
    assert_eq!(item["event"]["target_id"], id.as_str(), "{item}");
    let evs: Vec<EventRow> = s.get_json(&format!("/api/v1/events?target_id={id}")).await;
    assert_eq!(evs[0].action, "memory.create");
    let u: UndoResponse = s.post_json("/api/v1/undo", &serde_json::json!({"event_id": evs[0].id})).await;
    assert!(u.undone, "{}", u.message);
    let again = s.post("/api/v1/undo", &serde_json::json!({"event_id": evs[0].id})).await;
    assert_eq!(again.status(), 400);
    let again: serde_json::Value = again.json().await.unwrap();
    assert_eq!(again["kind"], "invalid");
    assert!(again["error"].as_str().unwrap().contains("already undone"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stream_sends_health_again_only_when_it_changes() {
    let s = Srv::start_with(ServiceOptions { health_tick: std::time::Duration::from_millis(100), ..Default::default() }).await;
    let mut stream = s.get("/api/v1/events/stream").await;
    let mut buf = String::new();
    // The stream opens with the health; start-up may still flip `index_ready` once.
    let mut first = next_item(&mut stream, &mut buf).await;
    assert_eq!(first["type"], "health");
    while first["health"]["index_ready"] != true {
        first = next_item(&mut stream, &mut buf).await;
    }
    assert_eq!(first["health"]["memories"], 0);
    // Nothing changes now: several ticks pass without another health item.
    let quiet = tokio::time::timeout(std::time::Duration::from_millis(500), next_item(&mut stream, &mut buf)).await;
    assert!(quiet.is_err(), "unchanged health was sent again: {quiet:?}");
    let _: RememberResponse = s.post_json("/api/v1/remember", &serde_json::json!({"content": "health moves"})).await;
    let changed = loop {
        let v = next_item(&mut stream, &mut buf).await;
        if v["type"] == "health" && v["health"]["memories"] == 1 { break v; }
    };
    assert_eq!(changed["health"]["memories"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn update_returns_its_event_id() {
    let s = Srv::start().await;
    let r: RememberResponse = s.post_json("/api/v1/remember", &serde_json::json!({"content": "archive me"})).await;
    let id = r.id.unwrap();
    let full: MemoryFull = s.post_json("/api/v1/update", &serde_json::json!({"id": id, "action": "archive"})).await;
    let ev = full.event_id.expect("update answers with its event id");
    let evs: Vec<EventRow> = s.get_json(&format!("/api/v1/events?target_id={id}")).await;
    assert_eq!((evs[0].id.as_str(), evs[0].action.as_str()), (ev.as_str(), "memory.archive"));
    let get: serde_json::Value = s.post_json("/api/v1/get", &serde_json::json!({"ids": [id]})).await;
    assert!(get["memories"][0].get("event_id").is_none(), "only update responses carry it");
}
```

(`the_stream_sends_health_again_only_when_it_changes` relies on nothing else changing health in an idle test service: the embed worker settles the one memory and `memories` stays 1. If `Health.memories` counts only active memories in your version, wait for the value the remembered memory produces instead of 1.) In `crates/alice/tests/cli.rs`, extend `every_subcommand_round_trips` with `history` (JSON array whose first item's `action` is `memory.create` for the remembered id) and `undo <that event>` (then `recall` no longer returns it).

Run: `nice -n 15 cargo test -j 6 --workspace` — Expected: pass; `pgrep -fl "alice daemon run"` empty afterwards.

- [ ] **Step 10: Commit**

```bash
nice -n 15 cargo fmt --all && nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
git add crates
git commit -m "feat: events with actors for every change, undo, history CLI and SSE stream (schema v2)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: Settings schema, layers, validation and the secrets store

Spec §6: one schema as Rust structs (serde + schemars); layers defaults → `~/.alice/config.toml` → `<repo>/.alice.toml` (project scope) → `ALICE_<SECTION>_<KEY>`; invalid values are rejected with the schema error and the previous value stays; secrets in `~/.alice/secrets.toml` (0600) by default, OS keychain opt-in, never returned by the API. This task builds the pure parts in `alice-core::settings` and the file-backed `SettingsHandle` and `SecretStore` in `alice-daemon`; Task 6 puts them behind the API, the CLI and the running service.

**Files:**
- Create: `crates/alice-core/src/settings.rs`
- Modify: `crates/alice-core/src/lib.rs` (`pub mod settings;`), `crates/alice-core/Cargo.toml` (`toml`, `toml_edit`), root `Cargo.toml`
- Modify: `crates/alice-core/src/search/fusion.rs` (`#[serde(default)]` on `Weights`; `HalfLives`), `crates/alice-core/src/search/mod.rs` (`SearchParams::half_lives`, used for recency)
- Create: `crates/alice-daemon/src/config.rs`, `crates/alice-daemon/src/secrets.rs`
- Modify: `crates/alice-daemon/src/lib.rs`, `crates/alice-daemon/src/paths.rs` (`secrets_file()`), `crates/alice-daemon/Cargo.toml` (`keyring` optional, feature `keychain`), `crates/alice/Cargo.toml` (enable `alice-daemon/keychain`)

**Interfaces:**
- Consumes: `alice_core::search::fusion::Weights`, `alice_core::model::Kind`, `alice_embed::MODEL_ID` (only in a doc comment; `alice-core` does not depend on `alice-embed`, so the default model id is the string literal).
- Produces:
  - `alice_core::settings::{Settings, StorageSettings, EmbeddingSettings, EmbeddingProvider, SearchSettings, CaptureSettings, JudgeSettings, JudgeProvider, JevSettings, DistillSettings, PrivacySettings, RedactMode, CustomPattern, SyncSettings, BackupSettings, CompatSettings, ServerSettings, McpSettings, SecretsSettings, SecretsBackend, MaintenanceSettings, HumanDuration, ByteSize, Retention, SettingError, Source, Resolved, KeyInfo, ProjectSettings, ProjectPrivacy}`
  - `alice_core::settings::{resolve(file: Option<&str>, env: &[(String, String)]) -> Result<Resolved, Vec<SettingError>>, leaves(v: &serde_json::Value) -> Vec<(String, serde_json::Value)>, get_key(s: &Settings, key: &str) -> Option<serde_json::Value>, edit_toml(text: &str, key: &str, value: &serde_json::Value) -> Result<String, SettingError>, key_infos() -> Vec<KeyInfo>, env_var(key: &str) -> String, load_project(root: &Path) -> Result<ProjectSettings, SettingError>, READ_ONLY_KEYS, RESTART_KEYS}`; `Resolved { settings, sources, warnings: Vec<String> }` (`warnings`: `ALICE_*` variables that are neither settings nor in `NON_SETTING_ENV`; they are ignored, never an error); `NON_SETTING_ENV: &[&str]`; `Settings::validate(&self) -> Vec<SettingError>`; `ProjectSettings::apply_to(&self, &PrivacySettings) -> PrivacySettings`
  - `alice_core::search::fusion::HalfLives { status, handoff, diary, note, fact, howto, bugfix, summary, decision, preference, warning, raw: f64 }` with `days(kind) -> Option<f64>` and `raw_days() -> Option<f64>` (0 = no decay); `SearchParams::half_lives: HalfLives`.
  - `alice_daemon::config::{SettingsHandle, PatchOutcome}`: `alice_daemon::config::{load_effective(home: &AliceHome) -> Effective, Effective { resolved: Resolved, errors: Vec<SettingError> }}` — the one "config.toml + `ALICE_*` + resolve + defaults" rule for code outside the running daemon (Task 7 spool redaction, Task 11 stdio MCP, Task 15 restore, Task 17 doctor); `SettingsHandle::{load(home: &AliceHome) -> Arc<Self>, load_with_env(home, env: Vec<(String, String)>) -> Arc<Self>, get(&self) -> Arc<Settings>, resolved(&self) -> Arc<Resolved>, errors(&self) -> Vec<SettingError>, warnings(&self) -> Vec<String>, subscribe(&self) -> broadcast::Receiver<Vec<String>>, patch(&self, changes: &serde_json::Map<String, serde_json::Value>) -> Result<PatchOutcome, Vec<SettingError>>, reload_if_changed(&self) -> Option<Vec<String>>, spawn_watcher(self: &Arc<Self>, every: Duration, stop: Arc<AtomicBool>) -> JoinHandle<()>}`; `PatchOutcome { changed, restart_required, overridden_by_env: Vec<String> }`
  - `alice_daemon::secrets::{SecretName, SecretStatus, SecretStore, FileSecrets, open_store(home: &AliceHome, backend: SecretsBackend) -> Arc<dyn SecretStore>, resolve(store: &dyn SecretStore, name: SecretName) -> Result<Option<(String, &'static str)>>, status(store: &dyn SecretStore, name: SecretName) -> SecretStatus, resolve_with/status_with(.., env: EnvLookup) (the same with the environment passed in, for tests), hint(value: &str) -> String}`; `SecretName::{JevApiKey ("jev_api_key", env TYPESAFE_API_KEY), EmbeddingApiKey ("embedding_api_key", env ALICE_EMBEDDING_API_KEY)}`
  - `AliceHome::secrets_file() -> PathBuf` (`<home>/secrets.toml`)

- [ ] **Step 1: Read the files this task modifies, as they are now.**

- [ ] **Step 2: Dependencies**

Root `[workspace.dependencies]`: `toml = "1.1.8"`, `toml_edit = "0.25.17"`, `keyring = { version = "4.2.0", default-features = false, features = ["v1"] }`. `alice-core`: `toml.workspace = true`, `toml_edit.workspace = true`. `alice-daemon`:

```toml
[features]
default = []
keychain = ["dep:keyring"]

[dependencies]
keyring = { workspace = true, optional = true }
toml.workspace = true
```

`crates/alice/Cargo.toml`: `alice-daemon = { path = "../alice-daemon", features = ["keychain"] }`.

- [ ] **Step 3: Write the failing settings tests**

`crates/alice-core/src/settings.rs`, test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn defaults_are_valid_and_match_the_spec() {
        let s = Settings::default();
        assert!(s.validate().is_empty(), "{:?}", s.validate());
        assert_eq!(s.judge.provider, JudgeProvider::Heuristic);
        assert_eq!((s.judge.auto_apply_threshold, s.judge.review_threshold), (0.85, 0.5));
        assert_eq!(s.judge.jev.model, "jev-1.13.0");
        assert_eq!(s.search.relevance_floor, 0.82);
        assert_eq!(s.storage.raw_retention, Retention::Forever);
        assert_eq!((s.backup.daily, s.backup.weekly), (7, 4));
        assert_eq!(s.secrets.backend, SecretsBackend::File);
        assert!(!s.mcp.admin_tools);
    }

    #[test]
    fn layers_apply_in_order_and_report_their_source() {
        let file = "[judge]\nprovider = \"jev\"\nreview_threshold = 0.6\n[search.weights]\nraw = 0.5\n";
        let r = resolve(Some(file), &env(&[("ALICE_JUDGE_REVIEW_THRESHOLD", "0.7"), ("ALICE_LOG", "debug")])).unwrap();
        assert_eq!(r.settings.judge.provider, JudgeProvider::Jev);
        assert_eq!(r.settings.judge.review_threshold, 0.7, "env wins over the file");
        assert_eq!(r.settings.search.weights.raw, 0.5);
        assert_eq!(r.settings.search.weights.knowledge, 1.0, "partial tables keep defaults");
        assert_eq!(r.sources["judge.provider"], Source::File);
        assert_eq!(r.sources["judge.review_threshold"], Source::Env { var: "ALICE_JUDGE_REVIEW_THRESHOLD".into() });
        assert_eq!(r.sources["judge.auto_apply_threshold"], Source::Default);
    }

    #[test]
    fn invalid_input_is_reported_with_its_key() {
        let cases: &[(&str, &[(&str, &str)], &str)] = &[
            ("[judge]\nprovder = \"jev\"\n", &[], "judge.provder"),
            ("[judge]\nauto_apply_threshold = \"high\"\n", &[], "judge.auto_apply_threshold"),
            ("[judge]\nauto_apply_threshold = 0.4\n", &[], "judge.review_threshold"),
            ("[privacy]\ncustom_patterns = [{name = \"x\", regex = \"(\"}]\n", &[], "privacy.custom_patterns"),
            ("[judge\n", &[], "config.toml"),
            ("", &[("ALICE_SEARCH_RELEVANCE_FLOOR", "very")], "search.relevance_floor"),
            ("[storage]\nraw_retention = \"soon\"\n", &[], "storage.raw_retention"),
            ("[embedding]\nprovider = \"local\"\nmodel = \"other\"\n", &[], "embedding.model"),
        ];
        for (file, vars, key) in cases {
            let errs = resolve(Some(file), &env(vars)).unwrap_err();
            assert!(errs.iter().any(|e| e.key == *key), "{file:?} {vars:?}: {errs:?}");
            assert!(errs.iter().all(|e| !e.message.is_empty()));
        }
    }

    #[test]
    fn unknown_alice_variables_are_ignored_with_a_warning() {
        let vars = env(&[
            ("ALICE_JUDGE_PROVDER", "jev"),             // a typo: warned, ignored
            ("ALICE_SOMETHING_ELSE", "1"),              // no such section: warned, ignored
            ("ALICE_JUDGE_REVIEW_THRESHOLD", "0.7"),    // a setting: applied
            ("ALICE_EMBEDDING_API_KEY", "sk-test-1234"), // a secret: recognised, not a setting
            ("ALICE_HOME", "/tmp/x"),
            ("ALICE_LOG", "debug"),
            ("ALICE_EMBEDDER", "fake"),
            ("TYPESAFE_API_KEY", "ts-test"),            // not ALICE_*: never looked at
        ]);
        let r = resolve(None, &vars).expect("unknown variables never make the configuration invalid");
        assert_eq!(r.settings.judge.review_threshold, 0.7, "the valid variable still applies");
        assert_eq!(r.settings.judge.provider, JudgeProvider::Heuristic, "the typo changes nothing");
        assert_eq!(r.warnings.len(), 2, "{:?}", r.warnings);
        assert!(r.warnings.iter().any(|w| w.contains("ALICE_JUDGE_PROVDER") && w.contains("alice config list")), "{:?}", r.warnings);
        assert!(r.warnings.iter().any(|w| w.contains("ALICE_SOMETHING_ELSE")), "{:?}", r.warnings);
        assert!(r.warnings.iter().all(|w| !w.contains("sk-test") && !w.contains("ts-test")), "values are never echoed");
        // With a file error as well, the environment's valid part is still what the daemon uses.
        let errs = resolve(Some("[judge\n"), &vars).unwrap_err();
        assert!(errs.iter().all(|e| !e.key.starts_with("ALICE_")), "{errs:?}");
    }

    #[test]
    fn edit_toml_keeps_comments_creates_tables_and_removes_keys() {
        let text = "# my settings\n[judge]\n# careful\nprovider = \"heuristic\"\n";
        let t = edit_toml(text, "judge.provider", &serde_json::json!("jev")).unwrap();
        assert!(t.contains("# careful") && t.contains("provider = \"jev\""), "{t}");
        let t = edit_toml(&t, "search.weights.raw", &serde_json::json!(0.4)).unwrap();
        assert_eq!(resolve(Some(&t), &[]).unwrap().settings.search.weights.raw, 0.4);
        let t = edit_toml(&t, "privacy.cloud_denied_projects", &serde_json::json!(["secret-proj"])).unwrap();
        assert_eq!(resolve(Some(&t), &[]).unwrap().settings.privacy.cloud_denied_projects, vec!["secret-proj"]);
        let t = edit_toml(&t, "judge.provider", &serde_json::Value::Null).unwrap();
        assert_eq!(resolve(Some(&t), &[]).unwrap().settings.judge.provider, JudgeProvider::Heuristic);
        assert!(edit_toml(&t, "judge.nope", &serde_json::json!(1)).is_err());
        assert!(edit_toml(&t, "storage.data_dir", &serde_json::json!("/x")).is_err(), "read-only");
    }

    #[test]
    fn human_units_round_trip() {
        for (s, d) in [("90d", 90 * 86_400), ("12h", 43_200), ("15m", 900), ("30s", 30), ("0", 0)] {
            let h = HumanDuration::parse(s).unwrap();
            assert_eq!(h.0.as_secs(), d);
            assert_eq!(HumanDuration::parse(&h.to_string()).unwrap(), h);
        }
        assert!(HumanDuration::parse("5 parsecs").is_err());
        assert_eq!(ByteSize::parse("10GB").unwrap().0, 10_000_000_000);
        assert_eq!(ByteSize::parse("1.5 MB").unwrap().0, 1_500_000);
        assert_eq!(Retention::parse("forever").unwrap(), Retention::Forever);
        assert_eq!(Retention::parse("90d").unwrap(), Retention::Age(std::time::Duration::from_secs(90 * 86_400)));
        assert_eq!(Retention::parse("10GB").unwrap(), Retention::Size(10_000_000_000));
        assert!(Retention::parse("0").is_err(), "0 would delete every transcript");
    }

    #[test]
    fn a_project_file_can_only_tighten_privacy() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join(".alice.toml"),
            "[privacy]\nblock_secrets = true\ncloud_denied = true\ncustom_patterns = [{name = \"ticket\", regex = \"TCK-[0-9]+\"}]\n").unwrap();
        let p = load_project(d.path()).unwrap();
        let global = PrivacySettings { redact: false, ..Default::default() };
        let eff = p.apply_to(&global);
        assert!(eff.redact, "a project file turns redaction on");
        assert_eq!(eff.redact_mode, RedactMode::Block);
        assert_eq!(eff.custom_patterns.len(), 1);
        assert!(p.privacy.cloud_denied);
        std::fs::write(d.path().join(".alice.toml"), "[privacy]\nredact = false\n").unwrap();
        assert!(load_project(d.path()).is_err(), "a project file cannot turn redaction off");
        assert_eq!(load_project(&d.path().join("missing")).unwrap(), ProjectSettings::default());
    }

    #[test]
    fn every_key_has_an_env_var_and_restart_keys_are_listed() {
        let keys = key_infos();
        assert!(keys.iter().any(|k| k.key == "judge.jev.rate_per_sec" && k.env_var == "ALICE_JUDGE_JEV_RATE_PER_SEC"));
        assert!(keys.iter().any(|k| k.key == "server.port" && k.restart));
        assert!(keys.iter().any(|k| k.key == "storage.data_dir" && k.read_only));
        assert!(keys.len() >= 50, "{}", keys.len());
    }
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-core settings::` — Expected: FAIL to compile.

- [ ] **Step 4: Implement `settings.rs`**

```rust
//! Settings (spec §6): one schema as Rust structs; layers defaults → `config.toml` →
//! environment (`ALICE_<SECTION>_<KEY>`), validated as a whole, so an invalid layer never
//! replaces a valid configuration. A repository's `.alice.toml` may only make Alice more
//! careful with that project's memories (`ProjectSettings`).

use crate::search::fusion::{HalfLives, Weights};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// Keys the API and `alice config set` refuse to change.
pub const READ_ONLY_KEYS: &[&str] = &["storage.data_dir"];
/// Keys that take effect only after `alice daemon stop` and a new start.
pub const RESTART_KEYS: &[&str] = &["server.port", "secrets.backend"];

// ---------------------------------------------------------------- units

/// A duration written `90d`, `12h`, `15m`, `30s`, or `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HumanDuration(pub Duration);

impl HumanDuration {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s == "0" {
            return Ok(Self(Duration::ZERO));
        }
        let (num, unit) = s.split_at(s.len().saturating_sub(1));
        let n: u64 = num.trim().parse().map_err(|_| format!("\"{s}\" is not a duration; write it like 90d, 12h, 15m or 30s"))?;
        let secs = match unit {
            "s" => n,
            "m" => n * 60,
            "h" => n * 3_600,
            "d" => n * 86_400,
            _ => return Err(format!("\"{s}\" is not a duration; write it like 90d, 12h, 15m or 30s")),
        };
        Ok(Self(Duration::from_secs(secs)))
    }
}

impl std::fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.0.as_secs();
        match s {
            0 => write!(f, "0"),
            _ if s % 86_400 == 0 => write!(f, "{}d", s / 86_400),
            _ if s % 3_600 == 0 => write!(f, "{}h", s / 3_600),
            _ if s % 60 == 0 => write!(f, "{}m", s / 60),
            _ => write!(f, "{s}s"),
        }
    }
}

/// Bytes written `10GB`, `500MB`, `1.5TB` (decimal units) or a plain number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

impl ByteSize {
    pub fn parse(s: &str) -> Result<Self, String> {
        let t = s.trim().to_ascii_uppercase().replace(' ', "");
        let split = t.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(t.len());
        let (num, unit) = t.split_at(split);
        let n: f64 = num.parse().map_err(|_| format!("\"{s}\" is not a size; write it like 10GB or 500MB"))?;
        let mult = match unit {
            "" | "B" => 1.0,
            "KB" => 1e3,
            "MB" => 1e6,
            "GB" => 1e9,
            "TB" => 1e12,
            _ => return Err(format!("\"{s}\" is not a size; write it like 10GB or 500MB")),
        };
        Ok(Self((n * mult).round() as u64))
    }
}

impl std::fmt::Display for ByteSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let b = self.0;
        for (unit, m) in [("TB", 1e12), ("GB", 1e9), ("MB", 1e6), ("KB", 1e3)] {
            if b as f64 >= m && (b as f64 / m).fract() == 0.0 {
                return write!(f, "{}{unit}", b as f64 / m);
            }
        }
        write!(f, "{b}B")
    }
}

/// How long raw transcripts are kept: `forever`, an age (`90d`) or a total size (`10GB`,
/// counted as the raw chunks' text bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Retention {
    #[default]
    Forever,
    Age(Duration),
    Size(u64),
}

impl Retention {
    pub fn parse(s: &str) -> Result<Self, String> {
        let t = s.trim();
        if t.eq_ignore_ascii_case("forever") {
            return Ok(Self::Forever);
        }
        if let Ok(d) = HumanDuration::parse(t)
            && !d.0.is_zero()
        {
            return Ok(Self::Age(d.0));
        }
        match ByteSize::parse(t) {
            // "0" would delete every transcript: never a default-looking value.
            Ok(b) if b.0 > 0 => Ok(Self::Size(b.0)),
            _ => Err(format!("\"{s}\" is not a retention; use forever, an age such as 90d, or a size such as 10GB")),
        }
    }
}

impl std::fmt::Display for Retention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Retention::Forever => write!(f, "forever"),
            Retention::Age(d) => write!(f, "{}", HumanDuration(*d)),
            Retention::Size(b) => write!(f, "{}", ByteSize(*b)),
        }
    }
}

macro_rules! string_serde {
    ($t:ty) => {
        impl Serialize for $t {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.to_string())
            }
        }
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let v = serde_json::Value::deserialize(d)?;
                let s = match &v {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Number(n) => n.to_string(),
                    _ => return Err(serde::de::Error::custom("expected a string")),
                };
                <$t>::parse(&s).map_err(serde::de::Error::custom)
            }
        }
    };
}
string_serde!(HumanDuration);
string_serde!(ByteSize);
string_serde!(Retention);

// ---------------------------------------------------------------- groups

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub storage: StorageSettings,
    pub embedding: EmbeddingSettings,
    pub search: SearchSettings,
    pub capture: CaptureSettings,
    pub judge: JudgeSettings,
    pub distill: DistillSettings,
    pub privacy: PrivacySettings,
    pub sync: SyncSettings,
    pub backup: BackupSettings,
    pub compat: CompatSettings,
    pub server: ServerSettings,
    pub mcp: McpSettings,
    pub secrets: SecretsSettings,
    pub maintenance: MaintenanceSettings,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StorageSettings {
    /// Where Alice keeps its data (read-only here: set the ALICE_HOME environment variable).
    pub data_dir: Option<String>,
    /// Raw transcripts: `forever`, an age such as `90d`, or a size such as `10GB`.
    #[schemars(with = "String")]
    pub raw_retention: Retention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingProvider {
    #[default]
    Local,
    Openai,
    Ollama,
    Voyage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddingSettings {
    pub provider: EmbeddingProvider,
    /// `multilingual-e5-small-q8` for `local`; the provider's model name otherwise.
    pub model: String,
    /// Base URL for HTTP providers (defaults per provider).
    pub endpoint: Option<String>,
    /// Texts per embedding request.
    pub batch_size: usize,
    /// Unload the local model after this long without use (`0` = never).
    #[schemars(with = "String")]
    pub idle_unload: HumanDuration,
}

impl Default for EmbeddingSettings {
    fn default() -> Self {
        Self {
            provider: EmbeddingProvider::Local,
            model: "multilingual-e5-small-q8".into(),
            endpoint: None,
            batch_size: 32,
            idle_unload: HumanDuration(Duration::from_secs(600)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SearchSettings {
    pub default_max_tokens: usize,
    pub relevance_floor: f32,
    pub raw_fallback_min_results: usize,
    pub weights: Weights,
    pub half_life: HalfLives,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            default_max_tokens: crate::model::limits::DEFAULT_RECALL_TOKENS,
            relevance_floor: crate::search::DEFAULT_RELEVANCE_FLOOR,
            raw_fallback_min_results: 3,
            weights: Weights::default(),
            half_life: HalfLives::default(),
        }
    }
}

/// Per-agent capture switches (used by SP2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureSettings {
    pub claude_code: bool,
    pub codex: bool,
    pub cursor: bool,
    pub windsurf: bool,
    pub opencode: bool,
    pub gemini: bool,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self { claude_code: true, codex: true, cursor: true, windsurf: true, opencode: true, gemini: true }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JudgeProvider {
    #[default]
    Heuristic,
    Jev,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct JevSettings {
    pub model: String,
    pub endpoint: String,
    /// Requests per second, at most 10.
    pub rate_per_sec: f64,
}

impl Default for JevSettings {
    fn default() -> Self {
        Self {
            model: "jev-1.13.0".into(),
            endpoint: "https://api.typesafe.ai/v1/systemone".into(),
            rate_per_sec: 10.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct JudgeSettings {
    pub provider: JudgeProvider,
    /// Decisions at or above this probability are applied without asking.
    pub auto_apply_threshold: f64,
    /// Decisions at or above this (and below auto) become review items; lower ones are dropped.
    pub review_threshold: f64,
    /// Hard stop for paid judges, per UTC day.
    pub daily_budget_usd: f64,
    pub timeout_ms: u64,
    /// Cosine similarity at which the weekly job suggests merging memories.
    pub cluster_similarity: f32,
    pub jev: JevSettings,
}

impl Default for JudgeSettings {
    fn default() -> Self {
        Self {
            provider: JudgeProvider::Heuristic,
            auto_apply_threshold: 0.85,
            review_threshold: 0.5,
            daily_budget_usd: 1.0,
            timeout_ms: 10_000,
            cluster_similarity: 0.92,
            jev: JevSettings::default(),
        }
    }
}

/// Distillation (used by SP2).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DistillSettings {
    pub provider: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RedactMode {
    /// Replace secrets with `[REDACTED:<type>]`.
    #[default]
    Redact,
    /// Refuse the write, naming the detector.
    Block,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CustomPattern {
    /// Shown as `[REDACTED:custom:<name>]`; lowercase letters, digits and `_`.
    pub name: String,
    /// A regular expression (Rust `regex` syntax).
    pub regex: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PrivacySettings {
    pub redact: bool,
    pub redact_mode: RedactMode,
    pub custom_patterns: Vec<CustomPattern>,
    /// Regular expressions; a match that one of them matches completely is kept.
    pub allowlist: Vec<String>,
    /// Project slugs never sent to a cloud judge or embedding provider.
    pub cloud_denied_projects: Vec<String>,
}

impl Default for PrivacySettings {
    fn default() -> Self {
        Self { redact: true, redact_mode: RedactMode::Redact, custom_patterns: Vec::new(), allowlist: Vec::new(), cloud_denied_projects: Vec::new() }
    }
}

/// Git sync (used by M4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SyncSettings {
    pub remote: Option<String>,
    pub branch: String,
    pub auto: String,
    pub include_raw: bool,
    pub excluded_projects: Vec<String>,
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self { remote: None, branch: "main".into(), auto: "off".into(), include_raw: false, excluded_projects: Vec::new() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BackupSettings {
    /// Daily backups kept.
    pub daily: u32,
    /// Weekly backups kept.
    pub weekly: u32,
    /// Backup directory (default `<data dir>/backups`).
    pub dir: Option<String>,
}

impl Default for BackupSettings {
    fn default() -> Self {
        Self { daily: 7, weekly: 4, dir: None }
    }
}

/// MemPalace compatibility layer (used by M3).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CompatSettings {
    pub mempalace: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ServerSettings {
    /// 0 = a free port, chosen at each start.
    pub port: u16,
    pub open_browser: bool,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self { port: 0, open_browser: true }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct McpSettings {
    /// Offer review_list, review_resolve, merge_memories and stats to agents.
    pub admin_tools: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SecretsBackend {
    /// `secrets.toml` in the data directory, readable by you only.
    #[default]
    File,
    /// The OS keychain (macOS Keychain, Windows Credential Manager, Secret Service).
    Keychain,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SecretsSettings {
    pub backend: SecretsBackend,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MaintenanceSettings {
    pub enabled: bool,
    /// Jobs start after the daemon has had no requests for this many seconds.
    pub idle_secs: u64,
    /// Forgotten memories are deleted for good after this many days.
    pub trash_days: u32,
    /// History events older than this many days are deleted.
    pub events_days: u32,
}

impl Default for MaintenanceSettings {
    fn default() -> Self {
        Self { enabled: true, idle_secs: 60, trash_days: 30, events_days: 365 }
    }
}

// ---------------------------------------------------------------- errors and sources

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SettingError {
    /// A dotted key (`judge.review_threshold`), `config.toml`, or an environment variable name.
    pub key: String,
    pub message: String,
}

fn err(key: impl Into<String>, message: impl Into<String>) -> SettingError {
    SettingError { key: key.into(), message: message.into() }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    Default,
    File,
    Env { var: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub settings: Settings,
    /// Where each leaf key's value came from.
    pub sources: BTreeMap<String, Source>,
    /// `ALICE_*` variables that were ignored (not a setting, not a known non-setting variable).
    pub warnings: Vec<String>,
}

/// `ALICE_*` variables read elsewhere that are not settings (never warned about). Secrets are
/// recognised here by name; their values are never read by `resolve`.
pub const NON_SETTING_ENV: &[&str] = &[
    "ALICE_HOME", "ALICE_LOG", "ALICE_EMBEDDER", "ALICE_AGENT", "ALICE_MODELS_DIR",
    "ALICE_NO_AUTOSTART", "ALICE_EVAL_OUT", "ALICE_TEST_BULK", "ALICE_EMBEDDING_API_KEY",
];

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct KeyInfo {
    pub key: String,
    pub env_var: String,
    pub read_only: bool,
    pub restart: bool,
}

pub fn env_var(key: &str) -> String {
    format!("ALICE_{}", key.replace('.', "_").to_ascii_uppercase())
}

/// Leaf keys (objects are walked; arrays, scalars and nulls are leaves), with their values.
pub fn leaves(v: &serde_json::Value) -> Vec<(String, serde_json::Value)> {
    fn walk(prefix: &str, v: &serde_json::Value, out: &mut Vec<(String, serde_json::Value)>) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, x) in m {
                    let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    walk(&key, x, out);
                }
            }
            other => out.push((prefix.to_string(), other.clone())),
        }
    }
    let mut out = Vec::new();
    walk("", v, &mut out);
    out
}

fn defaults_json() -> serde_json::Value {
    serde_json::to_value(Settings::default()).expect("settings serialise")
}

pub fn key_infos() -> Vec<KeyInfo> {
    leaves(&defaults_json())
        .into_iter()
        .map(|(key, _)| KeyInfo {
            env_var: env_var(&key),
            read_only: READ_ONLY_KEYS.contains(&key.as_str()),
            restart: RESTART_KEYS.contains(&key.as_str()),
            key,
        })
        .collect()
}

pub fn get_key(s: &Settings, key: &str) -> Option<serde_json::Value> {
    let v = serde_json::to_value(s).ok()?;
    key.split('.').try_fold(v, |v, part| v.get(part).cloned())
}

fn set_path(root: &mut serde_json::Value, key: &str, value: serde_json::Value) {
    let mut cur = root;
    let parts: Vec<&str> = key.split('.').collect();
    for p in &parts[..parts.len() - 1] {
        cur = cur
            .as_object_mut()
            .expect("objects along a known key")
            .entry(p.to_string())
            .or_insert_with(|| serde_json::json!({}));
    }
    cur[parts[parts.len() - 1]] = value;
}

/// Same JSON kind as the default (null defaults accept anything; numbers accept numbers).
fn same_kind(default: &serde_json::Value, given: &serde_json::Value) -> bool {
    use serde_json::Value::*;
    matches!(
        (default, given),
        (Null, _) | (Bool(_), Bool(_)) | (Number(_), Number(_)) | (String(_), String(_)) | (Array(_), Array(_))
    )
}

fn kind_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "nothing",
        serde_json::Value::Bool(_) => "true or false",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "a list",
        serde_json::Value::Object(_) => "a table",
    }
}

/// An environment value: TOML syntax if it parses (`0.7`, `true`, `["a"]`), else a string.
fn env_value(raw: &str) -> serde_json::Value {
    toml::from_str::<toml::Table>(&format!("v = {raw}"))
        .ok()
        .and_then(|t| t.get("v").cloned())
        .and_then(|v| serde_json::to_value(v).ok())
        .unwrap_or_else(|| serde_json::Value::String(raw.to_string()))
}

/// Merge the layers and validate the result as a whole.
pub fn resolve(file: Option<&str>, env: &[(String, String)]) -> Result<Resolved, Vec<SettingError>> {
    let defaults = defaults_json();
    let default_leaves: BTreeMap<String, serde_json::Value> = leaves(&defaults).into_iter().collect();
    let mut merged = defaults.clone();
    let mut sources: BTreeMap<String, Source> =
        default_leaves.keys().map(|k| (k.clone(), Source::Default)).collect();
    let mut errors = Vec::new();

    if let Some(text) = file.filter(|t| !t.trim().is_empty()) {
        match toml::from_str::<toml::Table>(text) {
            Err(e) => errors.push(err("config.toml", format!("{e}"))),
            Ok(table) => {
                let given = serde_json::to_value(table).expect("toml to json");
                for (key, value) in leaves(&given) {
                    match default_leaves.get(&key) {
                        None if key.is_empty() => {}
                        None => errors.push(err(&key, "not a setting; see `alice config list` for the keys")),
                        Some(_) if READ_ONLY_KEYS.contains(&key.as_str()) => {
                            errors.push(err(&key, "cannot be set in config.toml; set the ALICE_HOME environment variable instead"))
                        }
                        Some(d) if !same_kind(d, &value) => errors.push(err(
                            &key,
                            format!("must be {}, not {}", kind_name(d), kind_name(&value)),
                        )),
                        Some(_) => {
                            set_path(&mut merged, &key, value);
                            sources.insert(key, Source::File);
                        }
                    }
                }
            }
        }
    }

    let mut warnings = Vec::new();
    let env_keys: BTreeMap<String, String> =
        default_leaves.keys().map(|k| (env_var(k), k.clone())).collect();
    for (var, raw) in env {
        match env_keys.get(var) {
            Some(key) => {
                let value = env_value(raw);
                let d = &default_leaves[key];
                if !same_kind(d, &value) && !(d.is_string() || d.is_null()) {
                    errors.push(err(key, format!("{var} must be {}, not \"{raw}\"", kind_name(d))));
                } else {
                    let value = if d.is_string() && !value.is_string() { serde_json::Value::String(raw.clone()) } else { value };
                    set_path(&mut merged, key, value);
                    sources.insert(key.clone(), Source::Env { var: var.clone() });
                }
            }
            // Unknown variables never invalidate the configuration: a typo in one variable must
            // not send every setting back to its default. Values are never echoed (secrets).
            None if NON_SETTING_ENV.contains(&var.as_str()) => {}
            None if var.starts_with("ALICE_") => warnings.push(format!(
                "{var} is not a setting and is ignored; see `alice config list` for the variables"
            )),
            None => {}
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }
    let settings: Settings = match serde_json::from_value(merged.clone()) {
        Ok(s) => s,
        Err(e) => {
            // serde's message does not name the field: find the leaf that alone breaks it
            // (an unknown enum variant, a unit that does not parse).
            let key = leaves(&merged)
                .into_iter()
                .find(|(k, v)| {
                    let mut probe = defaults.clone();
                    set_path(&mut probe, k, v.clone());
                    serde_json::from_value::<Settings>(probe).is_err()
                })
                .map(|(k, _)| k)
                .unwrap_or_else(|| "config".to_string());
            return Err(vec![err(key, e.to_string())]);
        }
    };
    let invalid = settings.validate();
    if !invalid.is_empty() {
        return Err(invalid);
    }
    Ok(Resolved { settings, sources, warnings })
}

impl Settings {
    pub fn validate(&self) -> Vec<SettingError> {
        let mut e = Vec::new();
        let unit = |e: &mut Vec<SettingError>, key: &str, v: f64| {
            if !(0.0..=1.0).contains(&v) {
                e.push(err(key, format!("{v} is outside 0 to 1")));
            }
        };
        let j = &self.judge;
        unit(&mut e, "judge.auto_apply_threshold", j.auto_apply_threshold);
        unit(&mut e, "judge.review_threshold", j.review_threshold);
        if j.review_threshold > j.auto_apply_threshold {
            e.push(err("judge.review_threshold", format!(
                "{} is above judge.auto_apply_threshold ({}); the review threshold must not be higher",
                j.review_threshold, j.auto_apply_threshold
            )));
        }
        if j.daily_budget_usd < 0.0 {
            e.push(err("judge.daily_budget_usd", "must be 0 or more (0 stops paid judges)"));
        }
        if !(100..=120_000).contains(&j.timeout_ms) {
            e.push(err("judge.timeout_ms", "must be between 100 and 120000"));
        }
        if !(0.5..=1.0).contains(&j.cluster_similarity) {
            e.push(err("judge.cluster_similarity", "must be between 0.5 and 1"));
        }
        if !(j.jev.rate_per_sec > 0.0 && j.jev.rate_per_sec <= 10.0) {
            e.push(err("judge.jev.rate_per_sec", "must be above 0 and at most 10"));
        }
        if !(j.jev.endpoint.starts_with("https://") || j.jev.endpoint.starts_with("http://127.0.0.1")) {
            e.push(err("judge.jev.endpoint", "must be an https:// URL"));
        }
        if j.jev.model.trim().is_empty() {
            e.push(err("judge.jev.model", "must not be empty"));
        }
        let s = &self.search;
        unit(&mut e, "search.relevance_floor", f64::from(s.relevance_floor));
        if !(100..=50_000).contains(&s.default_max_tokens) {
            e.push(err("search.default_max_tokens", "must be between 100 and 50000"));
        }
        if s.raw_fallback_min_results > crate::model::limits::MAX_RECALL_LIMIT {
            e.push(err("search.raw_fallback_min_results", "must be at most 50"));
        }
        for (k, v) in leaves(&serde_json::to_value(&s.weights).unwrap_or_default())
            .into_iter()
            .chain(leaves(&serde_json::to_value(&s.half_life).unwrap_or_default()))
        {
            if v.as_f64().is_some_and(|x| x < 0.0) {
                e.push(err(format!("search.{k}"), "must be 0 or more"));
            }
        }
        let m = &self.embedding;
        if !(1..=256).contains(&m.batch_size) {
            e.push(err("embedding.batch_size", "must be between 1 and 256"));
        }
        if m.model.trim().is_empty() {
            e.push(err("embedding.model", "must not be empty"));
        }
        if m.provider == EmbeddingProvider::Local && m.model != "multilingual-e5-small-q8" {
            e.push(err("embedding.model", "the local provider runs multilingual-e5-small-q8 only; use provider openai, ollama or voyage for other models"));
        }
        if let Some(url) = &m.endpoint
            && !(url.starts_with("https://") || url.starts_with("http://"))
        {
            e.push(err("embedding.endpoint", "must be an http:// or https:// URL"));
        }
        let p = &self.privacy;
        for (i, c) in p.custom_patterns.iter().enumerate() {
            if c.name.is_empty() || !c.name.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_') {
                e.push(err("privacy.custom_patterns", format!("pattern {i}: name \"{}\" must be lowercase letters, digits and _", c.name)));
            }
            if let Err(x) = regex::Regex::new(&c.regex) {
                e.push(err("privacy.custom_patterns", format!("pattern {}: {x}", c.name)));
            }
        }
        for a in &p.allowlist {
            if let Err(x) = regex::Regex::new(a) {
                e.push(err("privacy.allowlist", format!("{a}: {x}")));
            }
        }
        if self.backup.daily < 1 {
            e.push(err("backup.daily", "keep at least 1 daily backup"));
        }
        let mt = &self.maintenance;
        if mt.idle_secs < 5 {
            e.push(err("maintenance.idle_secs", "must be at least 5"));
        }
        if mt.trash_days < 1 {
            e.push(err("maintenance.trash_days", "must be at least 1"));
        }
        if mt.events_days < 30 {
            e.push(err("maintenance.events_days", "must be at least 30 (undo needs the history)"));
        }
        e
    }
}

// ---------------------------------------------------------------- editing config.toml

fn json_to_toml(v: &serde_json::Value) -> Option<toml_edit::Value> {
    use serde_json::Value as J;
    Some(match v {
        J::Bool(b) => (*b).into(),
        J::Number(n) => match n.as_i64() {
            Some(i) => i.into(),
            None => n.as_f64()?.into(),
        },
        J::String(s) => s.as_str().into(),
        J::Array(a) => {
            let mut arr = toml_edit::Array::new();
            for x in a {
                arr.push(json_to_toml(x)?);
            }
            toml_edit::Value::Array(arr)
        }
        J::Object(m) => {
            let mut t = toml_edit::InlineTable::new();
            for (k, x) in m {
                t.insert(k, json_to_toml(x)?);
            }
            toml_edit::Value::InlineTable(t)
        }
        J::Null => return None,
    })
}

/// `text` with `key` set to `value` (removed when `value` is null), keeping comments and layout.
/// Only checks the key; the caller resolves the result to validate the value.
pub fn edit_toml(text: &str, key: &str, value: &serde_json::Value) -> Result<String, SettingError> {
    if READ_ONLY_KEYS.contains(&key) {
        return Err(err(key, "is read-only; set the ALICE_HOME environment variable to move the data directory"));
    }
    if !key_infos().iter().any(|k| k.key == key) {
        return Err(err(key, "not a setting; see `alice config list` for the keys"));
    }
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| err("config.toml", format!("cannot edit: {e}")))?;
    let parts: Vec<&str> = key.split('.').collect();
    let (last, tables) = parts.split_last().expect("non-empty key");
    let mut item = doc.as_item_mut();
    for t in tables {
        let table = item
            .as_table_like_mut()
            .ok_or_else(|| err(key, "config.toml has a value where a table is expected"))?;
        if table.get(t).is_none() {
            table.insert(t, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        item = table.get_mut(t).expect("inserted above");
    }
    let table = item
        .as_table_like_mut()
        .ok_or_else(|| err(key, "config.toml has a value where a table is expected"))?;
    match json_to_toml(value) {
        None => {
            table.remove(last);
        }
        Some(v) => {
            table.insert(last, toml_edit::Item::Value(v));
        }
    }
    Ok(doc.to_string())
}

// ---------------------------------------------------------------- project scope

/// `<repo>/.alice.toml`. Only settings that make Alice more careful are accepted.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectSettings {
    pub privacy: ProjectPrivacy,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectPrivacy {
    /// Refuse writes that contain secrets instead of redacting them.
    pub block_secrets: bool,
    /// Extra patterns redacted in this project.
    pub custom_patterns: Vec<CustomPattern>,
    /// Never send this project's memories to a cloud judge or embedding provider.
    pub cloud_denied: bool,
}

impl ProjectSettings {
    /// The project's privacy on top of the global one: never weaker.
    pub fn apply_to(&self, global: &PrivacySettings) -> PrivacySettings {
        let mut p = global.clone();
        let touched = self.privacy.block_secrets || !self.privacy.custom_patterns.is_empty();
        p.redact = p.redact || touched;
        if self.privacy.block_secrets {
            p.redact_mode = RedactMode::Block;
        }
        p.custom_patterns.extend(self.privacy.custom_patterns.iter().cloned());
        p
    }
}

/// A missing file is no project settings; an invalid one is an error naming the file.
pub fn load_project(root: &Path) -> Result<ProjectSettings, SettingError> {
    let path = root.join(".alice.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ProjectSettings::default()),
        Err(e) => return Err(err(path.display().to_string(), e.to_string())),
    };
    let p: ProjectSettings = toml::from_str(&text).map_err(|e| {
        err(path.display().to_string(), format!("{e}; a project file accepts only [privacy] block_secrets, custom_patterns and cloud_denied"))
    })?;
    let check = Settings { privacy: p.apply_to(&PrivacySettings::default()), ..Default::default() };
    match check.validate().into_iter().next() {
        Some(e) => Err(err(path.display().to_string(), e.message)),
        None => Ok(p),
    }
}
```

In `search/fusion.rs` add `#[serde(default)]` on `Weights` and:

```rust
/// Recency half-lives in days per kind (spec §4.4); 0 means no decay.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HalfLives {
    pub status: f64, pub handoff: f64, pub diary: f64, pub note: f64,
    pub fact: f64, pub howto: f64, pub bugfix: f64, pub summary: f64,
    pub decision: f64, pub preference: f64, pub warning: f64,
    /// Raw transcript chunks.
    pub raw: f64,
}

impl Default for HalfLives {
    fn default() -> Self {
        Self { status: 7.0, handoff: 14.0, diary: 30.0, note: 90.0, fact: 180.0, howto: 180.0,
               bugfix: 180.0, summary: 180.0, decision: 0.0, preference: 0.0, warning: 0.0,
               raw: crate::model::RAW_HALF_LIFE_DAYS }
    }
}

impl HalfLives {
    pub fn days(&self, k: Kind) -> Option<f64> {
        let d = match k {
            Kind::Status => self.status, Kind::Handoff => self.handoff, Kind::Diary => self.diary,
            Kind::Note => self.note, Kind::Fact => self.fact, Kind::Howto => self.howto,
            Kind::Bugfix => self.bugfix, Kind::Summary => self.summary, Kind::Decision => self.decision,
            Kind::Preference => self.preference, Kind::Warning => self.warning,
        };
        (d > 0.0).then_some(d)
    }
    pub fn raw_days(&self) -> Option<f64> {
        (self.raw > 0.0).then_some(self.raw)
    }
}
```

`SearchParams` gains `pub half_lives: HalfLives`; in `Filters::score`, replace `m.kind.half_life_days()` with `p.half_lives.days(m.kind)` and the raw half-life constant with `p.half_lives.raw_days()`. Add a fusion test: `HalfLives::default().days(k) == k.half_life_days()` for every `Kind` (defaults unchanged).

Run: `nice -n 15 cargo test -j 6 -p alice-core` — Expected: pass.

- [ ] **Step 5: Write the failing daemon-side tests (handle and secrets)**

`crates/alice-daemon/src/config.rs` tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> (tempfile::TempDir, AliceHome) {
        let d = tempfile::tempdir().unwrap();
        let h = AliceHome::new(d.path().to_path_buf());
        h.ensure_dirs().unwrap();
        (d, h)
    }

    #[test]
    fn an_invalid_file_at_start_means_defaults_and_a_visible_error() {
        let (_d, h) = home();
        std::fs::write(h.config_file(), "[judge]\nreview_threshold = 2\n").unwrap();
        let s = SettingsHandle::load_with_env(&h, vec![]);
        assert_eq!(s.get().judge.review_threshold, 0.5);
        assert!(s.errors().iter().any(|e| e.key == "judge.review_threshold"));
    }

    #[test]
    fn load_effective_reads_the_file_and_the_environment_like_the_daemon() {
        let (_d, h) = home();
        std::fs::write(h.config_file(), "[privacy]\ncloud_denied_projects = [\"p\"]\n").unwrap();
        let e = effective(&h.config_file(), &[("ALICE_JUDGE_REVIEW_THRESHOLD".into(), "0.7".into()), ("ALICE_NOPE".into(), "1".into())]);
        assert!(e.errors.is_empty(), "{:?}", e.errors);
        assert_eq!(e.resolved.settings.privacy.cloud_denied_projects, vec!["p"]);
        assert_eq!(e.resolved.settings.judge.review_threshold, 0.7);
        assert_eq!(e.resolved.warnings.len(), 1);
        std::fs::write(h.config_file(), "[judge\n").unwrap();
        let e = effective(&h.config_file(), &[("ALICE_JUDGE_REVIEW_THRESHOLD".into(), "0.7".into())]);
        assert_eq!(e.errors[0].key, "config.toml");
        assert_eq!(e.resolved.settings.judge.review_threshold, 0.7, "a broken file keeps the valid environment");
        let s = SettingsHandle::load_with_env(&h, vec![("ALICE_NOPE".into(), "1".into())]);
        assert_eq!(s.warnings().len(), 1, "the handle reports ignored variables too");
    }

    #[test]
    fn patch_validates_writes_the_file_and_notifies() {
        let (_d, h) = home();
        std::fs::write(h.config_file(), "# keep me\n").unwrap();
        let s = SettingsHandle::load_with_env(&h, vec![("ALICE_SEARCH_RELEVANCE_FLOOR".into(), "0.8".into())]);
        let mut rx = s.subscribe();
        let mut m = serde_json::Map::new();
        m.insert("judge.review_threshold".into(), serde_json::json!(0.6));
        m.insert("search.relevance_floor".into(), serde_json::json!(0.9));
        m.insert("server.port".into(), serde_json::json!(4545));
        let out = s.patch(&m).unwrap();
        assert_eq!(s.get().judge.review_threshold, 0.6);
        assert_eq!(s.get().search.relevance_floor, 0.8, "the environment still wins");
        assert_eq!(out.overridden_by_env, vec!["search.relevance_floor".to_string()]);
        assert_eq!(out.restart_required, vec!["server.port".to_string()]);
        let text = std::fs::read_to_string(h.config_file()).unwrap();
        assert!(text.contains("# keep me") && text.contains("review_threshold = 0.6"), "{text}");
        assert!(rx.try_recv().unwrap().contains(&"judge.review_threshold".to_string()));

        let mut bad = serde_json::Map::new();
        bad.insert("judge.auto_apply_threshold".into(), serde_json::json!(0.1));
        let errs = s.patch(&bad).unwrap_err();
        assert_eq!(errs[0].key, "judge.review_threshold");
        assert_eq!(std::fs::read_to_string(h.config_file()).unwrap(), text, "file untouched");
        let mut ro = serde_json::Map::new();
        ro.insert("storage.data_dir".into(), serde_json::json!("/tmp"));
        assert!(s.patch(&ro).is_err());
    }

    #[test]
    fn file_edits_apply_and_a_half_written_file_keeps_the_last_good_settings() {
        let (_d, h) = home();
        let s = SettingsHandle::load_with_env(&h, vec![]);
        std::fs::write(h.config_file(), "[judge]\nprovider = \"jev\"\n").unwrap();
        bump_mtime(&h.config_file());
        assert_eq!(s.reload_if_changed().unwrap(), vec!["judge.provider".to_string()]);
        assert_eq!(s.get().judge.provider, JudgeProvider::Jev);
        std::fs::write(h.config_file(), "[judge]\nprovider = \"je").unwrap();
        bump_mtime(&h.config_file());
        assert!(s.reload_if_changed().is_none());
        assert_eq!(s.get().judge.provider, JudgeProvider::Jev, "last good settings stay");
        assert!(s.errors().iter().any(|e| e.key == "config.toml"));
    }

    #[test]
    fn the_watcher_picks_up_an_edit() {
        let (_d, h) = home();
        let s = SettingsHandle::load_with_env(&h, vec![]);
        let stop = Arc::new(AtomicBool::new(false));
        let t = s.spawn_watcher(Duration::from_millis(50), stop.clone());
        std::fs::write(h.config_file(), "[mcp]\nadmin_tools = true\n").unwrap();
        bump_mtime(&h.config_file());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !s.get().mcp.admin_tools {
            assert!(std::time::Instant::now() < deadline, "watcher did not reload");
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::SeqCst);
        t.join().unwrap();
    }

    /// Some file systems keep 1 s mtimes: make each write look new.
    fn bump_mtime(p: &std::path::Path) {
        let f = std::fs::File::options().write(true).open(p).unwrap();
        let t = std::time::SystemTime::now() + Duration::from_secs(rand_secs());
        f.set_modified(t).unwrap();
    }

    fn rand_secs() -> u64 {
        use std::sync::atomic::AtomicU64;
        static N: AtomicU64 = AtomicU64::new(1);
        N.fetch_add(1, Ordering::SeqCst)
    }
}
```

`crates/alice-daemon/src/secrets.rs` tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_store_sets_gets_deletes_and_stays_private() {
        let d = tempfile::tempdir().unwrap();
        let store = FileSecrets::new(d.path().join("secrets.toml"));
        assert_eq!(store.get(SecretName::JevApiKey).unwrap(), None);
        store.set(SecretName::JevApiKey, "ts_live_abcdef1234567890").unwrap();
        store.set(SecretName::EmbeddingApiKey, "sk-other").unwrap();
        assert_eq!(store.get(SecretName::JevApiKey).unwrap().as_deref(), Some("ts_live_abcdef1234567890"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.path().join("secrets.toml")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(store.delete(SecretName::JevApiKey).unwrap());
        assert!(!store.delete(SecretName::JevApiKey).unwrap());
        assert_eq!(store.get(SecretName::EmbeddingApiKey).unwrap().as_deref(), Some("sk-other"));
        assert!(store.set(SecretName::JevApiKey, "").is_err());
        assert!(store.set(SecretName::JevApiKey, "two\nlines").is_err());
    }

    #[test]
    fn status_shows_only_a_hint_and_the_environment_wins() {
        // The environment is passed in, so the test does not depend on (or touch) the real
        // TYPESAFE_API_KEY / ALICE_EMBEDDING_API_KEY of the machine running it.
        let d = tempfile::tempdir().unwrap();
        let store = FileSecrets::new(d.path().join("secrets.toml"));
        store.set(SecretName::EmbeddingApiKey, "ts_live_abcdef1234567890").unwrap();
        let no_env = |_: &str| None;
        let s = status_with(&store, SecretName::EmbeddingApiKey, &no_env);
        assert!(s.set);
        assert_eq!((s.hint.as_deref(), s.source.as_deref()), (Some("••••7890"), Some("file")));
        // The variable wins over the store, and only its hint is shown.
        let env = |var: &str| (var == "ALICE_EMBEDDING_API_KEY").then(|| " sk-from-env-5555 ".to_string());
        let s = status_with(&store, SecretName::EmbeddingApiKey, &env);
        assert_eq!((s.hint.as_deref(), s.source.as_deref()), (Some("••••5555"), Some("env")));
        assert_eq!(resolve_with(&store, SecretName::EmbeddingApiKey, &env).unwrap(), Some(("sk-from-env-5555".to_string(), "env")));
        // An empty variable does not count.
        let empty = |_: &str| Some("  ".to_string());
        assert_eq!(status_with(&store, SecretName::EmbeddingApiKey, &empty).source.as_deref(), Some("file"));
        // Jev's key is read from TYPESAFE_API_KEY.
        let ts = |var: &str| (var == "TYPESAFE_API_KEY").then(|| "ts-env-9999".to_string());
        assert_eq!(status_with(&store, SecretName::JevApiKey, &ts).source.as_deref(), Some("env"));
        assert_eq!(hint("short"), "••••");
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("abcdef"), "{json}");
        assert_eq!(SecretName::parse("jev_api_key"), Some(SecretName::JevApiKey));
        assert_eq!(SecretName::parse("nope"), None);
    }
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-daemon config:: secrets::` — Expected: FAIL to compile.

- [ ] **Step 6: Implement `config.rs` and `secrets.rs`**

`crates/alice-daemon/src/config.rs`:

```rust
//! The daemon's live settings: `config.toml` + environment, re-read when the file changes
//! (polled), changed through `patch` (which edits the file and keeps its comments). An invalid
//! file or patch never replaces valid settings; the errors are kept for `alice config list`,
//! `/api/v1/settings` and `alice doctor`.

use crate::paths::AliceHome;
use alice_core::settings::{self, Resolved, SettingError, Settings, Source};
use parking_lot::{Mutex, RwLock};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

pub use alice_core::settings::JudgeProvider;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

fn stamp(path: &std::path::Path) -> Option<Stamp> {
    let m = std::fs::metadata(path).ok()?;
    Some(Stamp { modified: m.modified().ok(), len: m.len() })
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct PatchOutcome {
    /// Keys whose effective value changed.
    pub changed: Vec<String>,
    /// Keys written that apply only after a daemon restart.
    pub restart_required: Vec<String>,
    /// Keys written to config.toml but overridden by an environment variable.
    pub overridden_by_env: Vec<String>,
}

pub struct SettingsHandle {
    file: PathBuf,
    env: Vec<(String, String)>,
    state: RwLock<Arc<Resolved>>,
    errors: RwLock<Vec<SettingError>>,
    seen: Mutex<Option<Stamp>>,
    write: Mutex<()>,
    changes: tokio::sync::broadcast::Sender<Vec<String>>,
}

fn default_resolved() -> Resolved {
    settings::resolve(None, &[]).expect("defaults are valid")
}

fn process_env() -> Vec<(String, String)> {
    std::env::vars().filter(|(k, _)| k.starts_with("ALICE_")).collect()
}

/// Settings as a process outside the running daemon sees them.
pub struct Effective {
    pub resolved: Resolved,
    /// Problems in `config.toml` or a known variable; when non-empty, `resolved` is the defaults
    /// plus the valid environment (or the bare defaults), as the daemon itself would start.
    pub errors: Vec<SettingError>,
}

/// `config.toml` + the process's `ALICE_*` variables, resolved over the defaults. Never fails.
/// The one rule for code that cannot ask the daemon: spool redaction (Task 7), stdio MCP
/// (Task 11), restore (Task 15) and doctor (Task 17). The daemon loads the same way.
pub fn load_effective(home: &AliceHome) -> Effective {
    effective(&home.config_file(), &process_env())
}

fn effective(file: &std::path::Path, env: &[(String, String)]) -> Effective {
    let text = std::fs::read_to_string(file).ok();
    let out = match settings::resolve(text.as_deref(), env) {
        Ok(resolved) => Effective { resolved, errors: Vec::new() },
        Err(errors) => {
            for e in &errors {
                tracing::warn!(key = %e.key, message = %e.message, "invalid setting; using defaults until it is fixed");
            }
            // Defaults plus a valid environment, if the file is the problem.
            let resolved = settings::resolve(None, env).unwrap_or_else(|_| default_resolved());
            Effective { resolved, errors }
        }
    };
    for w in &out.resolved.warnings {
        tracing::warn!("{w}");
    }
    out
}

fn changed_keys(a: &Settings, b: &Settings) -> Vec<String> {
    let a = settings::leaves(&serde_json::to_value(a).expect("json"));
    let b: std::collections::BTreeMap<_, _> =
        settings::leaves(&serde_json::to_value(b).expect("json")).into_iter().collect();
    a.into_iter().filter(|(k, v)| b.get(k) != Some(v)).map(|(k, _)| k).collect()
}

impl SettingsHandle {
    /// The settings for `home` with the process's `ALICE_*` variables. Never fails.
    pub fn load(home: &AliceHome) -> Arc<Self> {
        Self::load_with_env(home, process_env())
    }

    pub fn load_with_env(home: &AliceHome, env: Vec<(String, String)>) -> Arc<Self> {
        let file = home.config_file();
        let Effective { resolved, errors } = effective(&file, &env);
        let (changes, _) = tokio::sync::broadcast::channel(16);
        Arc::new(Self {
            seen: Mutex::new(stamp(&file)),
            file,
            env,
            state: RwLock::new(Arc::new(resolved)),
            errors: RwLock::new(errors),
            write: Mutex::new(()),
            changes,
        })
    }

    pub fn get(&self) -> Arc<Settings> {
        Arc::new(self.state.read().settings.clone())
    }

    pub fn resolved(&self) -> Arc<Resolved> {
        self.state.read().clone()
    }

    pub fn errors(&self) -> Vec<SettingError> {
        self.errors.read().clone()
    }

    /// Ignored `ALICE_*` variables (reported in health and `alice config list`).
    pub fn warnings(&self) -> Vec<String> {
        self.state.read().warnings.clone()
    }

    /// Changed keys, after every change that took effect.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Vec<String>> {
        self.changes.subscribe()
    }

    fn swap(&self, r: Resolved) -> Vec<String> {
        let changed = changed_keys(&self.state.read().settings, &r.settings);
        *self.state.write() = Arc::new(r);
        self.errors.write().clear();
        if !changed.is_empty() {
            let _ = self.changes.send(changed.clone());
        }
        changed
    }

    /// Re-read `config.toml` if it changed on disk. `Some(changed keys)` when new settings took
    /// effect; `None` when nothing changed or the file is invalid (the last good settings stay).
    pub fn reload_if_changed(&self) -> Option<Vec<String>> {
        let _w = self.write.lock();
        let now = stamp(&self.file);
        if *self.seen.lock() == now {
            return None;
        }
        *self.seen.lock() = now;
        let text = std::fs::read_to_string(&self.file).ok();
        match settings::resolve(text.as_deref(), &self.env) {
            Ok(r) => Some(self.swap(r)),
            Err(errs) => {
                for e in &errs {
                    tracing::warn!(key = %e.key, message = %e.message, "config.toml is invalid; keeping the previous settings");
                }
                *self.errors.write() = errs;
                None
            }
        }
    }

    /// Apply dotted-key changes (`null` removes a key from config.toml). All or nothing.
    pub fn patch(&self, changes: &serde_json::Map<String, serde_json::Value>) -> Result<PatchOutcome, Vec<SettingError>> {
        let _w = self.write.lock();
        let mut text = std::fs::read_to_string(&self.file).unwrap_or_default();
        for (k, v) in changes {
            text = settings::edit_toml(&text, k, v).map_err(|e| vec![e])?;
        }
        let r = settings::resolve(Some(&text), &self.env)?;
        let tmp = self.file.with_extension(format!("toml.{}.tmp", alice_core::ids::new_id()));
        let write = (|| -> std::io::Result<()> {
            std::fs::write(&tmp, text.as_bytes())?;
            std::fs::File::open(&tmp)?.sync_all()?;
            std::fs::rename(&tmp, &self.file)
        })();
        if let Err(e) = write {
            let _ = std::fs::remove_file(&tmp);
            return Err(vec![SettingError { key: "config.toml".into(), message: format!("cannot write {}: {e}", self.file.display()) }]);
        }
        *self.seen.lock() = stamp(&self.file);
        let overridden_by_env = changes
            .keys()
            .filter(|k| matches!(r.sources.get(k.as_str()), Some(Source::Env { .. })))
            .cloned()
            .collect();
        let restart_required = changes
            .keys()
            .filter(|k| settings::RESTART_KEYS.contains(&k.as_str()))
            .cloned()
            .collect();
        let changed = self.swap(r);
        Ok(PatchOutcome { changed, restart_required, overridden_by_env })
    }

    /// Poll `config.toml` every `every` until `stop` is set.
    pub fn spawn_watcher(self: &Arc<Self>, every: Duration, stop: Arc<AtomicBool>) -> JoinHandle<()> {
        let me = self.clone();
        std::thread::Builder::new()
            .name("alice-config".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Some(changed) = me.reload_if_changed() {
                        tracing::info!(?changed, "settings reloaded from config.toml");
                    }
                    let until = std::time::Instant::now() + every;
                    while std::time::Instant::now() < until && !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(every.min(Duration::from_millis(50)));
                    }
                }
            })
            .expect("spawn config watcher")
    }
}
```

`crates/alice-daemon/src/secrets.rs`:

```rust
//! Secrets (spec §6): `secrets.toml` readable only by its owner (default), or the OS keychain
//! when `secrets.backend = "keychain"`. Values never leave this module except to the provider
//! that needs them; the API sees only `SecretStatus`.

use crate::paths::AliceHome;
use alice_core::error::{AliceError, Result};
use alice_core::settings::SecretsBackend;
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SecretName {
    /// TypeSafe Jev (judge).
    JevApiKey,
    /// The HTTP embedding provider (openai, voyage).
    EmbeddingApiKey,
}

impl SecretName {
    pub const ALL: [SecretName; 2] = [SecretName::JevApiKey, SecretName::EmbeddingApiKey];
    pub fn as_str(self) -> &'static str {
        match self {
            SecretName::JevApiKey => "jev_api_key",
            SecretName::EmbeddingApiKey => "embedding_api_key",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|n| n.as_str() == s)
    }
    /// An environment variable that overrides the stored value.
    pub fn env_var(self) -> &'static str {
        match self {
            SecretName::JevApiKey => "TYPESAFE_API_KEY",
            SecretName::EmbeddingApiKey => "ALICE_EMBEDDING_API_KEY",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SecretStatus {
    pub set: bool,
    /// The last four characters, e.g. `••••ab12`.
    pub hint: Option<String>,
    /// `file`, `keychain` or `env`.
    pub source: Option<String>,
}

pub trait SecretStore: Send + Sync {
    fn get(&self, name: SecretName) -> Result<Option<String>>;
    fn set(&self, name: SecretName, value: &str) -> Result<()>;
    fn delete(&self, name: SecretName) -> Result<bool>;
    fn backend(&self) -> &'static str;
}

pub fn hint(value: &str) -> String {
    let n = value.chars().count();
    if n <= 8 {
        "••••".into()
    } else {
        format!("••••{}", value.chars().skip(n - 4).collect::<String>())
    }
}

fn check_value(value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 4_096 || value.contains(['\n', '\r']) {
        return Err(AliceError::invalid(
            "a secret must be one line of 1 to 4096 characters; paste the key again",
        ));
    }
    Ok(())
}

pub struct FileSecrets {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileSecrets {
    pub fn new(path: PathBuf) -> Self {
        Self { path, lock: Mutex::new(()) }
    }

    fn read(&self) -> Result<toml::Table> {
        match std::fs::read_to_string(&self.path) {
            Ok(t) => toml::from_str(&t).map_err(|e| AliceError::Other(format!(
                "{} is not valid TOML ({e}); fix or delete it, then set the secrets again", self.path.display()
            ))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// Written to a new private file, synced, then renamed over the old one.
    fn write(&self, t: &toml::Table) -> Result<()> {
        let tmp = self.path.with_extension(format!("toml.{}.tmp", alice_core::ids::new_id()));
        let text = toml::to_string(t).map_err(|e| AliceError::Other(e.to_string()))?;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let done = (|| -> std::io::Result<()> {
            let mut f = opts.open(&tmp)?;
            std::io::Write::write_all(&mut f, text.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, &self.path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
            }
            Ok(())
        })();
        if done.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        Ok(done?)
    }
}

impl SecretStore for FileSecrets {
    fn get(&self, name: SecretName) -> Result<Option<String>> {
        let _l = self.lock.lock();
        Ok(self.read()?.get(name.as_str()).and_then(|v| v.as_str()).map(String::from))
    }
    fn set(&self, name: SecretName, value: &str) -> Result<()> {
        check_value(value)?;
        let _l = self.lock.lock();
        let mut t = self.read()?;
        t.insert(name.as_str().into(), toml::Value::String(value.trim().into()));
        self.write(&t)
    }
    fn delete(&self, name: SecretName) -> Result<bool> {
        let _l = self.lock.lock();
        let mut t = self.read()?;
        let had = t.remove(name.as_str()).is_some();
        if had {
            self.write(&t)?;
        }
        Ok(had)
    }
    fn backend(&self) -> &'static str {
        "file"
    }
}

/// OS keychain (service `alice`, account = the secret's name). Opt-in: a macOS Keychain
/// prompt after every rebuilt binary would block a background daemon.
#[cfg(feature = "keychain")]
pub struct KeychainSecrets;

#[cfg(feature = "keychain")]
impl KeychainSecrets {
    fn entry(name: SecretName) -> Result<keyring::Entry> {
        keyring::Entry::new("alice", name.as_str()).map_err(|e| AliceError::Other(format!(
            "the OS keychain is unavailable ({e}); set secrets.backend = \"file\" to use secrets.toml"
        )))
    }
}

#[cfg(feature = "keychain")]
impl SecretStore for KeychainSecrets {
    fn get(&self, name: SecretName) -> Result<Option<String>> {
        match Self::entry(name)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(AliceError::Other(format!("reading {} from the keychain failed: {e}", name.as_str()))),
        }
    }
    fn set(&self, name: SecretName, value: &str) -> Result<()> {
        check_value(value)?;
        Self::entry(name)?.set_password(value.trim()).map_err(|e| AliceError::Other(format!(
            "storing {} in the keychain failed: {e}", name.as_str()
        )))
    }
    fn delete(&self, name: SecretName) -> Result<bool> {
        match Self::entry(name)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(AliceError::Other(format!("deleting {} from the keychain failed: {e}", name.as_str()))),
        }
    }
    fn backend(&self) -> &'static str {
        "keychain"
    }
}

pub fn open_store(home: &AliceHome, backend: SecretsBackend) -> Arc<dyn SecretStore> {
    match backend {
        #[cfg(feature = "keychain")]
        SecretsBackend::Keychain => Arc::new(KeychainSecrets),
        #[cfg(not(feature = "keychain"))]
        SecretsBackend::Keychain => {
            tracing::warn!("this build has no keychain support; using secrets.toml");
            Arc::new(FileSecrets::new(home.secrets_file()))
        }
        SecretsBackend::File => Arc::new(FileSecrets::new(home.secrets_file())),
    }
}

/// Reads one environment variable (the process environment, or a test's stand-in).
pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

fn process_env(var: &str) -> Option<String> {
    std::env::var(var).ok()
}

/// The value to use and where it came from: the environment variable wins over the store.
pub fn resolve(store: &dyn SecretStore, name: SecretName) -> Result<Option<(String, &'static str)>> {
    resolve_with(store, name, &process_env)
}

pub fn resolve_with(store: &dyn SecretStore, name: SecretName, env: EnvLookup<'_>) -> Result<Option<(String, &'static str)>> {
    if let Some(v) = env(name.env_var()).filter(|v| !v.trim().is_empty()) {
        return Ok(Some((v.trim().to_string(), "env")));
    }
    Ok(store.get(name)?.map(|v| (v, store.backend())))
}

pub fn status(store: &dyn SecretStore, name: SecretName) -> SecretStatus {
    status_with(store, name, &process_env)
}

pub fn status_with(store: &dyn SecretStore, name: SecretName, env: EnvLookup<'_>) -> SecretStatus {
    match resolve_with(store, name, env) {
        Ok(Some((v, source))) => SecretStatus { set: true, hint: Some(hint(&v)), source: Some(source.into()) },
        _ => SecretStatus { set: false, hint: None, source: None },
    }
}
```

`paths.rs`: `pub fn secrets_file(&self) -> PathBuf { self.root.join("secrets.toml") }`. `lib.rs`: `pub mod config; pub mod secrets;`.

Run: `nice -n 15 cargo test -j 6 -p alice-daemon config:: secrets::` — Expected: pass.

- [ ] **Step 7: Commit**

```bash
nice -n 15 cargo fmt --all && nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
nice -n 15 cargo test -j 6 --workspace
git add Cargo.toml Cargo.lock crates
git commit -m "feat: settings schema with layers and validation; secrets file (0600) and opt-in keychain

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 6: Live settings — HTTP API, `alice config`, wiring into the service, reload

The future web UI's contract (spec §6, §8.4): every setting readable and writable through the API with validation and live reload; secrets write-only. This task puts `SettingsHandle` and the secret store into `Service`, reads search settings per request, records `settings.change` / `secret.set` / `secret.delete` events (never values), streams `StreamItem::Settings`, and adds the routes and `alice config`.

**Files:**
- Modify: `crates/alice-daemon/src/service.rs` (settings, secrets, watcher, per-request search params, `settings_view`, `patch_settings`, `set_secret`, `delete_secret`, `test_provider`)
- Modify: `crates/alice-daemon/src/writer.rs` (`WriteOp::RecordEvent`)
- Modify: `crates/alice-daemon/src/daemon.rs` (load settings first; `embedding.idle_unload` and `server.port` from settings)
- Modify: `crates/alice-daemon/src/http.rs`, `crates/alice-daemon/src/client.rs`
- Modify: `crates/alice-core/src/api.rs` (`SettingsView`, `SecretRequest`, `ProviderTest`, `Health.settings_warnings`)
- Modify: `crates/alice-daemon/src/config.rs` (`follow_settings`, `changed_keys` crate-visible)
- Modify: `crates/alice/src/bench.rs` (drops the removed `ServiceOptions` search fields, if it sets them)
- Modify: `crates/alice-core/src/settings.rs` (`RESTART_KEYS` gains the embedding keys until Task 16; `parse_cli_value`)
- Modify: `crates/alice/src/main.rs`, `crates/alice/src/cmd.rs` (`alice config …`)
- Test: service tests, `crates/alice-daemon/tests/http_api.rs`, `crates/alice/tests/cli.rs`

**Interfaces:**
- Consumes: Task 5 (`SettingsHandle`, `PatchOutcome`, `SecretStore`, `SecretName`, `secrets::{open_store, status}`), Task 4 (`events`, `StreamItem`).
- Produces:
  - `ServiceOptions { reader_pool: usize, settings: Option<Arc<SettingsHandle>>, settings_poll: Duration, health_tick: Duration }` (the M1 fields `relevance_floor`, `raw_min_results`, `weights` are removed: they are settings now; Task 4's `health_tick` stays).
  - `Health::settings_warnings: Vec<String>` (`#[serde(default, skip_serializing_if = "Vec::is_empty")]`; ignored `ALICE_*` variables from `SettingsHandle::warnings()`, Task 5); `alice status` prints them.
  - `Service::{settings(&self) -> Arc<Settings>, settings_handle(&self) -> &Arc<SettingsHandle>, secrets(&self) -> &Arc<dyn SecretStore>, settings_view(&self) -> SettingsView, patch_settings(&self, changes: serde_json::Map<String, serde_json::Value>) -> Result<PatchResponse, Vec<SettingError>>, set_secret(&self, name: SecretName, value: String) -> Result<SecretStatus>, delete_secret(&self, name: SecretName) -> Result<bool>, test_provider(&self, which: &str) -> Result<ProviderTest>}`
  - `alice_core::api::{SettingsView { settings: serde_json::Value, sources: BTreeMap<String, Source>, keys: Vec<KeyInfo>, errors: Vec<SettingError>, warnings: Vec<String>, secrets: BTreeMap<String, SecretStatus>, restart_pending: Vec<String> }, PatchResponse { outcome: PatchOutcome-shaped fields changed/restart_required/overridden_by_env, view: SettingsView }, SecretRequest { value: String }, ProviderTest { ok: bool, message: String, latency_ms: u64 }}` (`SecretStatus` and `PatchOutcome` move to `alice-core::api` so the client can deserialise them; `alice-daemon` re-exports them).
  - `WriteOp::RecordEvent { actor: Actor, action: String, target_type: String, target_id: String, before: Option<serde_json::Value>, after: Option<serde_json::Value> }`
  - HTTP: `GET /api/v1/settings`, `PATCH /api/v1/settings` (body: object of dotted keys; `null` removes the key from config.toml), `GET /api/v1/settings/schema`, `PUT /api/v1/secrets/{name}` (`{"value": …}`), `DELETE /api/v1/secrets/{name}`, `POST /api/v1/settings/test/{provider}` (`judge` | `embedding`). Validation failures answer 400 `{"kind":"invalid","error":<first message>,"errors":[{key,message}…]}`.
  - Client: `DaemonClient::{settings, patch_settings, settings_schema, set_secret, delete_secret, test_provider}`.
  - CLI: `alice config list`, `alice config get <key>`, `alice config set <key> <value>`, `alice config unset <key>`, `alice config edit`, `alice config set-secret <name>` (stdin, echo off on a terminal), `alice config delete-secret <name>`, `alice config test <judge|embedding>`.
  - `alice_core::settings::parse_cli_value(raw: &str) -> serde_json::Value` (TOML literal if it parses, else a string; the same rule as environment values).

- [ ] **Step 1: Read the files this task modifies, as they are now.**

- [ ] **Step 2: Restart keys and CLI values**

In `settings.rs`: `pub const RESTART_KEYS: &[&str] = &["server.port", "secrets.backend", "embedding.idle_unload", "embedding.batch_size", "embedding.provider", "embedding.model", "embedding.endpoint"];` with a comment "Task 16 makes provider/model/endpoint apply live through a re-embed", and rename `env_value` to `pub fn parse_cli_value` (keep `env_value` as a call to it). Adjust `every_key_has_an_env_var_and_restart_keys_are_listed` if it lists restart keys.

- [ ] **Step 3: Write the failing service tests**

In `service.rs` tests (adapt `svc()` to `ServiceOptions::default()`, which now loads settings from the temp home):

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn search_settings_apply_to_the_next_recall_without_a_restart() {
        let (d, s) = svc().await;
        let cwd = repo(&d, "proj");
        s.remember(RememberRequest { content: "lease timeouts in the hub writer".into(), cwd: cwd.clone(), ..Default::default() }).await.unwrap();
        wait_embedded(&s).await;
        let q = || RecallRequest { query: "writer hub lease timeout problem".into(), cwd: cwd.clone(), format: OutputFormat::Json, ..Default::default() };
        let before = s.recall(q()).await.unwrap().results;
        assert!(!before.is_empty() && before.iter().all(|h| h.score > 0.0), "found, with a score, before the change: {before:?}");
        let before = before.len();
        let mut m = serde_json::Map::new();
        m.insert("search.weights.knowledge".into(), serde_json::json!(0.0));
        let out = s.patch_settings(m).unwrap();
        assert_eq!(out.changed, vec!["search.weights.knowledge".to_string()]);
        let after = s.recall(q()).await.unwrap();
        assert!(!after.results.is_empty(), "the memory is still returned (weights scale scores; they do not filter)");
        assert!(after.results.iter().all(|h| h.score == 0.0), "knowledge weight 0 zeroes knowledge scores: {before} → {:?}", after.results);
        // The settings event is written fire-and-forget by the config-events thread: wait for it.
        let ev = wait_events(&s, "config", |ev| ev.iter().any(|e| e.action == "settings.change")).await;
        assert!(ev[0].after.as_ref().unwrap().to_string().contains("search.weights.knowledge"));
    }

    /// Events for `target`, polled until `ok` holds (at most 5 s): writes queued with
    /// `send_nowait` land a moment after the call that caused them returns.
    async fn wait_events(s: &Service, target: &str, ok: impl Fn(&[EventRow]) -> bool) -> Vec<EventRow> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let ev = s.events(EventsRequest { target_id: Some(target.into()), ..Default::default() }).await.unwrap();
            if ok(&ev) {
                return ev;
            }
            assert!(std::time::Instant::now() < deadline, "no matching event for {target}: {ev:?}");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn secrets_are_write_only_and_their_events_carry_no_value() {
        let (_d, s) = svc().await;
        let st = s.set_secret(SecretName::EmbeddingApiKey, "sk-test-0123456789abcd".into()).unwrap();
        assert_eq!((st.set, st.hint.as_deref()), (true, Some("••••abcd")));
        let view = s.settings_view();
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("0123456789"), "{json}");
        assert!(view.secrets["embedding_api_key"].set);
        let ev = wait_events(&s, "embedding_api_key", |ev| ev.iter().any(|e| e.action == "secret.set")).await;
        assert_eq!(ev[0].action, "secret.set");
        assert!(!serde_json::to_string(&ev).unwrap().contains("0123456789"));
        assert!(s.delete_secret(SecretName::EmbeddingApiKey).unwrap());
        assert!(!s.settings_view().secrets["embedding_api_key"].set);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_invalid_config_edit_keeps_the_running_settings_and_shows_the_error() {
        let (d, s) = svc().await;
        let home = AliceHome::new(d.path().to_path_buf());
        std::fs::write(home.config_file(), "[judge]\nreview_threshold = 0.9\nauto_apply_threshold = 0.6\n").unwrap();
        assert!(s.settings_handle().reload_if_changed().is_none());
        assert_eq!(s.settings().judge.review_threshold, 0.5);
        let view = s.settings_view();
        assert!(view.errors.iter().any(|e| e.key == "judge.review_threshold"), "{:?}", view.errors);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ignored_variables_show_in_health_and_the_settings_view() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        home.ensure_dirs().unwrap();
        let handle = SettingsHandle::load_with_env(&home, vec![("ALICE_NOPE".into(), "1".into())]);
        let s = Service::start(home, &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions { settings: Some(handle), ..Default::default() }).unwrap();
        assert!(s.health().await.unwrap().settings_warnings.iter().any(|w| w.contains("ALICE_NOPE")));
        assert!(s.settings_view().warnings.iter().any(|w| w.contains("ALICE_NOPE")));
        s.shutdown();
    }
```

Run: `nice -n 15 cargo test -j 6 -p alice-daemon service::` — Expected: FAIL to compile.

- [ ] **Step 4: Wire settings into the service**

`ServiceOptions`:

```rust
#[derive(Clone)]
pub struct ServiceOptions {
    /// Most read connections open at once (each has its own page cache); further reads wait.
    pub reader_pool: usize,
    /// Settings to use; `None` loads `config.toml` and `ALICE_*` from the home.
    pub settings: Option<Arc<SettingsHandle>>,
    /// How often `config.toml` is checked for edits.
    pub settings_poll: Duration,
    /// How often the SSE stream checks whether health changed (Task 4).
    pub health_tick: Duration,
}

impl Default for ServiceOptions {
    fn default() -> Self {
        Self { reader_pool: 8, settings: None, settings_poll: Duration::from_secs(2), health_tick: Duration::from_secs(5) }
    }
}
```

In `Service` add `settings: Arc<SettingsHandle>`, `secrets: Arc<dyn SecretStore>`, `started_with: Arc<Settings>`, and a third thread handle (the config watcher) in `threads`. In `start`:

```rust
        let settings = opts.settings.clone().unwrap_or_else(|| SettingsHandle::load(&home));
        let started_with = settings.get();
        let secrets = crate::secrets::open_store(&home, started_with.secrets.backend);
        let watcher = settings.spawn_watcher(opts.settings_poll, stop.clone());
        // Each change that takes effect: an event and a stream item.
        {
            let rx = settings.subscribe();
            let start = settings.get();
            // Weak: the loop must not keep the handle (and so the channel) alive after shutdown.
            let weak = Arc::downgrade(&settings);
            let writer = writer.clone();
            std::thread::Builder::new().name("alice-config-events".into()).spawn(move || {
                crate::config::follow_settings(
                    rx,
                    start,
                    move || weak.upgrade().map(|h| h.get()).unwrap_or_default(),
                    move |changed| {
                        writer.send_nowait(WriteOp::RecordEvent {
                            actor: Actor::User,
                            action: "settings.change".into(),
                            target_type: "settings".into(),
                            target_id: "config".into(),
                            before: None,
                            after: Some(serde_json::json!({ "changed": changed })),
                        });
                    },
                )
            })?;
        }
```

In `config.rs` (make `changed_keys` `pub(crate)`):

```rust
/// The settings-change loop: `on_change(changed keys)` for every change that took effect.
/// After missed notifications (`Lagged`) it diffs the current settings against the last ones it
/// saw — it never stops on a lag; it ends only when the channel closes.
pub(crate) fn follow_settings(
    mut rx: tokio::sync::broadcast::Receiver<Vec<String>>,
    start: Arc<Settings>,
    current: impl Fn() -> Arc<Settings>,
    mut on_change: impl FnMut(Vec<String>),
) {
    use tokio::sync::broadcast::error::RecvError;
    let mut last = start;
    loop {
        let changed = match rx.blocking_recv() {
            Ok(changed) => changed,
            Err(RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "settings notifications lagged; re-reading the current settings");
                changed_keys(&last, &current())
            }
            Err(RecvError::Closed) => break,
        };
        last = current();
        if !changed.is_empty() {
            on_change(changed);
        }
    }
}
```

Later tasks that react to changed keys (Task 9's judge rebuild, Task 16's embedding switch) do it inside this `on_change` closure (or a second `follow_settings` loop), never with their own `while let Ok(..) = rx.blocking_recv()`. Test in `config.rs` tests:

```rust
    #[test]
    fn the_settings_loop_rereads_after_a_lag_and_ends_only_on_close() {
        let (tx, rx) = tokio::sync::broadcast::channel::<Vec<String>>(1);
        let mut now = Settings::default();
        now.judge.review_threshold = 0.6;
        let now = Arc::new(now);
        for k in ["a", "b", "c"] {
            tx.send(vec![k.to_string()]).unwrap(); // capacity 1: the loop starts lagged
        }
        drop(tx);
        let mut seen = Vec::new();
        follow_settings(rx, Arc::new(Settings::default()), || now.clone(), |c| seen.push(c));
        assert_eq!(seen, vec![vec!["judge.review_threshold".to_string()], vec!["c".to_string()]]);
    }
```

(The watcher thread joins in `shutdown` after `stop` is set.) Add the accessors from the Interfaces list. `health()` sets `settings_warnings: self.settings.warnings()`; `alice status` prints each warning on its own line (`warning: …`). In `recall`, build the search parameters from the settings of this request:

```rust
        let cfg = self.settings();
        let max_tokens = req.max_tokens.unwrap_or(cfg.search.default_max_tokens);
        // … inside the closure:
                    relevance_floor: cfg.search.relevance_floor,
                    raw_min_results: cfg.search.raw_fallback_min_results,
                    half_lives: cfg.search.half_life.clone(),
        // … and pass `&cfg.search.weights` to `search::run` instead of `opts.weights`.
```

`writer.rs`: add `WriteOp::RecordEvent { … }` whose `apply` calls `record(conn, st, EventInput { … })` (Task 4's helper) and returns `WriteOutcome::Done`. When `action == "settings.change"`, also push `StreamItem::Settings { changed }` (from `after.changed`) to the batch's broadcast list.

`settings_view`:

```rust
    pub fn settings_view(&self) -> SettingsView {
        let r = self.settings.resolved();
        let running = &self.started_with;
        let restart_pending = alice_core::settings::RESTART_KEYS
            .iter()
            .filter(|k| get_key(&r.settings, k) != get_key(running, k))
            .map(|k| k.to_string())
            .collect();
        SettingsView {
            settings: serde_json::to_value(&r.settings).expect("settings serialise"),
            sources: r.sources.clone(),
            keys: alice_core::settings::key_infos(),
            errors: self.settings.errors(),
            warnings: self.settings.warnings(),
            secrets: SecretName::ALL
                .iter()
                .map(|n| (n.as_str().to_string(), crate::secrets::status(self.secrets.as_ref(), *n)))
                .collect(),
            restart_pending,
        }
    }

    pub fn patch_settings(&self, changes: serde_json::Map<String, serde_json::Value>) -> std::result::Result<PatchResponse, Vec<SettingError>> {
        let out = self.settings.patch(&changes)?;
        Ok(PatchResponse {
            changed: out.changed,
            restart_required: out.restart_required,
            overridden_by_env: out.overridden_by_env,
            view: self.settings_view(),
        })
    }

    pub fn set_secret(&self, name: SecretName, value: String) -> Result<SecretStatus> {
        self.secrets.set(name, &value)?;
        self.writer.send_nowait(WriteOp::RecordEvent {
            actor: Actor::User, action: "secret.set".into(), target_type: "secret".into(),
            target_id: name.as_str().into(), before: None, after: None,
        });
        Ok(crate::secrets::status(self.secrets.as_ref(), name))
    }

    pub fn delete_secret(&self, name: SecretName) -> Result<bool> {
        let had = self.secrets.delete(name)?;
        if had {
            self.writer.send_nowait(WriteOp::RecordEvent {
                actor: Actor::User, action: "secret.delete".into(), target_type: "secret".into(),
                target_id: name.as_str().into(), before: None, after: None,
            });
        }
        Ok(had)
    }

    /// Connection test for a provider. Task 9 adds `judge` with Jev; Task 16 HTTP embeddings.
    pub async fn test_provider(&self, which: &str) -> Result<ProviderTest> {
        let started = std::time::Instant::now();
        let ok = |message: String| ProviderTest { ok: true, message, latency_ms: started.elapsed().as_millis() as u64 };
        match which {
            "embedding" => {
                let e = self.embedder.clone();
                let r = tokio::task::spawn_blocking(move || e.embed_passages(&["connection test".to_string()])).await.map_err(join_err)?;
                match r {
                    Ok(v) if v.len() == 1 => Ok(ok(format!("{} answered with {} dimensions", self.embedder.model_id(), v[0].len()))),
                    Ok(_) => Ok(ProviderTest { ok: false, message: "the embedder returned no vector".into(), latency_ms: started.elapsed().as_millis() as u64 }),
                    Err(e) => Ok(ProviderTest { ok: false, message: e.to_string(), latency_ms: started.elapsed().as_millis() as u64 }),
                }
            }
            "judge" => Ok(ok("the heuristic judge needs no connection".into())),
            other => Err(AliceError::invalid(format!("unknown provider \"{other}\"; test judge or embedding"))),
        }
    }
```

`daemon.rs`: load `let settings = SettingsHandle::load(&home);` before the embedder; the `OnnxOptions.idle_unload` comes from `settings.get().embedding.idle_unload.0` (`Duration::ZERO` → `None`); `serve` binds `settings.get().server.port` when `DaemonOptions.port == 0`; pass `ServiceOptions { settings: Some(settings), ..Default::default() }`. Remove the `ServiceOptions { relevance_floor, … }` uses in `bench.rs` and tests.

Run: `nice -n 15 cargo test -j 6 -p alice-daemon service::` — Expected: pass.

- [ ] **Step 5: HTTP routes and client**

`http.rs` (guarded router):

```rust
        .route("/api/v1/settings", get(get_settings).patch(patch_settings))
        .route("/api/v1/settings/schema", get(settings_schema))
        .route("/api/v1/settings/test/{provider}", post(test_provider))
        .route("/api/v1/secrets/{name}", axum::routing::put(put_secret).delete(delete_secret))
```

```rust
#[derive(Serialize)]
struct SettingsErrors {
    error: String,
    kind: &'static str,
    errors: Vec<SettingError>,
}

async fn get_settings(State(st): State<AppState>) -> Json<SettingsView> {
    Json(st.svc.settings_view())
}

async fn patch_settings(
    State(st): State<AppState>,
    ApiJson(body): ApiJson<serde_json::Map<String, serde_json::Value>>,
) -> Response {
    match st.svc.patch_settings(body) {
        Ok(r) => Json(r).into_response(),
        Err(errors) => (
            StatusCode::BAD_REQUEST,
            Json(SettingsErrors {
                error: errors.first().map(|e| format!("{}: {}", e.key, e.message)).unwrap_or_default(),
                kind: "invalid",
                errors,
            }),
        )
            .into_response(),
    }
}

async fn settings_schema() -> Json<serde_json::Value> {
    Json(serde_json::to_value(schemars::schema_for!(alice_core::settings::Settings)).unwrap_or_default())
}

fn secret_name(name: &str) -> Result<SecretName, ApiError> {
    SecretName::parse(name).ok_or_else(|| ApiError(AliceError::invalid(format!(
        "unknown secret \"{name}\"; use one of: {}",
        SecretName::ALL.iter().map(|n| n.as_str()).collect::<Vec<_>>().join(", ")
    ))))
}

async fn put_secret(
    State(st): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    ApiJson(req): ApiJson<SecretRequest>,
) -> ApiResult<SecretStatus> {
    Ok(Json(st.svc.set_secret(secret_name(&name)?, req.value)?))
}

async fn delete_secret(
    State(st): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> ApiResult<serde_json::Value> {
    let deleted = st.svc.delete_secret(secret_name(&name)?)?;
    Ok(Json(serde_json::json!({ "deleted": deleted })))
}

async fn test_provider(
    State(st): State<AppState>,
    axum::extract::Path(provider): axum::extract::Path<String>,
) -> ApiResult<ProviderTest> {
    Ok(Json(st.svc.test_provider(&provider).await?))
}
```

(`axum::routing::put`/`.delete` method routing on one path; enable the axum `macros` feature only if a route needs it — these do not.) Client:

```rust
    pub async fn settings(&self) -> Result<SettingsView> {
        self.get_json("/api/v1/settings").await
    }
    pub async fn settings_schema(&self) -> Result<serde_json::Value> {
        self.get_json("/api/v1/settings/schema").await
    }
    /// Validation failures come back as `AliceError::Invalid` with every `key: message` on its own line.
    pub async fn patch_settings(&self, changes: &serde_json::Map<String, serde_json::Value>) -> Result<PatchResponse> {
        let resp = self.http.patch(format!("{}/api/v1/settings", self.base))
            .bearer_auth(&self.token).json(changes).send().await.map_err(transport)?;
        if resp.status() == reqwest::StatusCode::BAD_REQUEST {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            let lines: Vec<String> = body["errors"].as_array().into_iter().flatten()
                .map(|e| format!("{}: {}", e["key"].as_str().unwrap_or("?"), e["message"].as_str().unwrap_or("")))
                .collect();
            return Err(AliceError::Invalid(if lines.is_empty() { body["error"].as_str().unwrap_or("invalid settings").into() } else { lines.join("\n") }));
        }
        Self::decode(resp).await
    }
    pub async fn set_secret(&self, name: &str, value: &str) -> Result<SecretStatus> {
        let resp = self.http.put(format!("{}/api/v1/secrets/{name}", self.base))
            .bearer_auth(&self.token).json(&SecretRequest { value: value.into() }).send().await.map_err(transport)?;
        Self::decode(resp).await
    }
    pub async fn delete_secret(&self, name: &str) -> Result<bool> {
        let resp = self.http.delete(format!("{}/api/v1/secrets/{name}", self.base))
            .bearer_auth(&self.token).send().await.map_err(transport)?;
        let v: serde_json::Value = Self::decode(resp).await?;
        Ok(v["deleted"].as_bool().unwrap_or(false))
    }
    pub async fn test_provider(&self, which: &str) -> Result<ProviderTest> {
        self.call(&format!("/api/v1/settings/test/{which}"), &serde_json::json!({}), Duration::from_secs(30)).await
    }
```

HTTP test in `crates/alice-daemon/tests/http_api.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn settings_api_round_trip_validation_schema_and_secrets() {
    let s = Srv::start().await;
    let view: serde_json::Value = s.get_json("/api/v1/settings").await;
    assert_eq!(view["settings"]["judge"]["provider"], "heuristic");
    assert!(view["keys"].as_array().unwrap().len() >= 50);
    let schema: serde_json::Value = s.get_json("/api/v1/settings/schema").await;
    assert!(schema["properties"]["judge"].is_object());
    let ok: serde_json::Value = ok_json(s.patch("/api/v1/settings", &serde_json::json!({"judge.review_threshold": 0.6})).await).await;
    assert_eq!(ok["view"]["settings"]["judge"]["review_threshold"], 0.6);
    let bad = s.patch("/api/v1/settings", &serde_json::json!({"judge.review_threshold": 0.95})).await;
    assert_eq!(bad.status(), 400);
    let body: serde_json::Value = bad.json().await.unwrap();
    assert_eq!(body["errors"][0]["key"], "judge.review_threshold");
    let put: serde_json::Value = ok_json(s.put("/api/v1/secrets/jev_api_key", &serde_json::json!({"value": "ts_secret_value_1234"})).await).await;
    assert_eq!(put["hint"], "••••1234");
    let view = s.get("/api/v1/settings").await.text().await.unwrap();
    assert!(!view.contains("ts_secret_value"), "the API never returns a secret");
    let unknown = s.put("/api/v1/secrets/github", &serde_json::json!({"value": "x"})).await;
    assert_eq!(unknown.status(), 400);
    let gone = s.delete("/api/v1/secrets/jev_api_key").await;
    assert!(gone.status().is_success(), "{}", gone.status());
}
```

(Uses Task 4's `Srv` helpers — `get_json`, `get`, `patch`, `put`, `delete`, `ok_json` — and adds none.)

- [ ] **Step 6: `alice config`**

`main.rs`: `#[command(subcommand)] Config(cmd::ConfigCmd)` with doc "Read and change settings (config.toml, applied live) and secrets". `cmd.rs`:

```rust
#[derive(Subcommand)]
pub enum ConfigCmd {
    /// Every setting with its value and where it comes from, then problems and secrets.
    List,
    /// One setting's value.
    Get { key: String },
    /// Change a setting (written to config.toml, applied at once unless it says otherwise).
    Set { key: String, value: String },
    /// Remove a setting from config.toml (back to its default).
    Unset { key: String },
    /// Open config.toml in $EDITOR; problems are shown afterwards.
    Edit,
    /// Store a secret read from stdin (never from the command line).
    SetSecret { name: String },
    DeleteSecret { name: String },
    /// Check that a provider answers.
    Test { provider: String },
}
```

```rust
        Command::Config(c) => config(&home, c, json).await,
```

```rust
async fn config(home: &AliceHome, c: ConfigCmd, json: bool) -> Result<()> {
    let client = client(home).await?;
    match c {
        ConfigCmd::List => {
            let v = client.settings().await?;
            print(json, &v, || {
                let mut out = String::new();
                for (key, value) in alice_core::settings::leaves(&v.settings) {
                    let src = match v.sources.get(&key) {
                        Some(Source::File) => " (config.toml)".to_string(),
                        Some(Source::Env { var }) => format!(" (${var})"),
                        _ => String::new(),
                    };
                    out.push_str(&format!("{key} = {value}{src}\n"));
                }
                for e in &v.errors {
                    out.push_str(&format!("PROBLEM {}: {}\n", e.key, e.message));
                }
                for k in &v.restart_pending {
                    out.push_str(&format!("RESTART {k} changes after `alice daemon stop` and the next start\n"));
                }
                for (name, s) in &v.secrets {
                    out.push_str(&format!("secret {name}: {}\n", if s.set { format!("set {} ({})", s.hint.clone().unwrap_or_default(), s.source.clone().unwrap_or_default()) } else { "not set".into() }));
                }
                out
            })
        }
        ConfigCmd::Get { key } => {
            let v = client.settings().await?;
            let value = key.split('.').try_fold(v.settings.clone(), |v, p| v.get(p).cloned())
                .with_context(|| format!("no setting {key}; see `alice config list`"))?;
            print(json, &value, || format!("{value}\n"))
        }
        ConfigCmd::Set { key, value } => patch(&client, key, alice_core::settings::parse_cli_value(&value), json).await,
        ConfigCmd::Unset { key } => patch(&client, key, serde_json::Value::Null, json).await,
        ConfigCmd::Edit => {
            let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR"))
                .context("set $EDITOR (or $VISUAL) to edit config.toml, or use `alice config set`")?;
            let status = std::process::Command::new(editor).arg(home.config_file()).status()?;
            anyhow::ensure!(status.success(), "the editor exited with {status}");
            tokio::time::sleep(Duration::from_millis(2_500)).await; // one poll of the watcher
            let v = client.settings().await?;
            for e in &v.errors {
                eprintln!("PROBLEM {}: {} (the previous settings stay in effect)", e.key, e.message);
            }
            Ok(())
        }
        ConfigCmd::SetSecret { name } => {
            let value = read_secret(&format!("Paste {name} and press Enter: "))?;
            let s = client.set_secret(&name, &value).await?;
            print(json, &s, || format!("{name} set ({})\n", s.hint.clone().unwrap_or_default()))
        }
        ConfigCmd::DeleteSecret { name } => {
            let had = client.delete_secret(&name).await?;
            print(json, &serde_json::json!({"deleted": had}), || if had { format!("{name} deleted\n") } else { format!("{name} was not set\n") })
        }
        ConfigCmd::Test { provider } => {
            let r = client.test_provider(&provider).await?;
            print(json, &r, || format!("{} {} ({} ms)\n", if r.ok { "OK" } else { "FAILED" }, r.message, r.latency_ms))?;
            anyhow::ensure!(r.ok, "the {provider} provider test failed");
            Ok(())
        }
    }
}

async fn patch(client: &DaemonClient, key: String, value: serde_json::Value, json: bool) -> Result<()> {
    let mut m = serde_json::Map::new();
    m.insert(key.clone(), value);
    let r = client.patch_settings(&m).await?;
    print(json, &r, || {
        let mut out = if r.changed.is_empty() { format!("{key}: no change\n") } else { format!("{key}: applied\n") };
        if !r.restart_required.is_empty() { out.push_str("takes effect after `alice daemon stop` and the next start\n"); }
        if !r.overridden_by_env.is_empty() { out.push_str(&format!("note: ${} overrides it\n", alice_core::settings::env_var(&key))); }
        out
    })
}

/// One line from stdin; on a Unix terminal the input is not echoed.
fn read_secret(prompt: &str) -> Result<String> {
    use std::io::{BufRead, IsTerminal, Write};
    let stdin = std::io::stdin();
    let tty = stdin.is_terminal();
    if tty {
        eprint!("{prompt}");
        std::io::stderr().flush()?;
    }
    #[cfg(unix)]
    let restore = if tty {
        // SAFETY: tcgetattr/tcsetattr on our own stdin with a zeroed termios they fill.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            libc::tcgetattr(0, &mut t);
            let old = t;
            t.c_lflag &= !libc::ECHO;
            libc::tcsetattr(0, libc::TCSANOW, &t);
            Some(old)
        }
    } else {
        None
    };
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    #[cfg(unix)]
    if let Some(old) = restore {
        // SAFETY: restores the attributes saved above.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &old) };
        eprintln!();
    }
    read?;
    let v = line.trim().to_string();
    anyhow::ensure!(!v.is_empty(), "no secret was given; paste it on stdin");
    Ok(v)
}
```

Process test (unix) in `crates/alice/tests/cli.rs`:

```rust
#[test]
fn config_set_get_secret_and_live_reload() {
    let env = Env::new();
    env.ok(&["remember", "seed"]);
    env.ok(&["config", "set", "judge.review_threshold", "0.6"]);
    assert_eq!(env.json(&["config", "get", "judge.review_threshold"]), serde_json::json!(0.6));
    let bad = env.run(&["config", "set", "judge.review_threshold", "0.99"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("judge.review_threshold"));
    let out = env.run_with_stdin(&["config", "set-secret", "embedding_api_key"], "sk-cli-secret-98765\n");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let list = env.ok(&["config", "list"]);
    assert!(list.contains("embedding_api_key: set ••••8765") && !list.contains("sk-cli-secret"), "{list}");
    // A direct edit of config.toml applies without a restart.
    std::fs::write(env.home.join("config.toml"), "[mcp]\nadmin_tools = true\n").unwrap();
    eventually(|| env.json(&["config", "get", "mcp.admin_tools"]) == serde_json::json!(true), "live reload");
    let log = env.log();
    assert!(!log.contains("sk-cli-secret"), "the secret never reaches the log");
}
```

- [ ] **Step 7: Run everything and measure the idle unload**

```bash
export S=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2 ALICE_HOME=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2/home; mkdir -p "$ALICE_HOME"; [ -e "$ALICE_HOME/models" ] || ln -s "$HOME/.alice/models" "$ALICE_HOME/models"   # scratch prelude (Global Constraints)
nice -n 15 cargo test -j 6 --workspace
pgrep -fl "alice daemon run" || true
```

Expected: pass, no daemon left. Then measure Task 2's idle unload on the bench data (one heavy run). `ALICE_HOME` is the bench data for **every** command of this measurement (status, recall and stop included), so nothing reaches the real `~/.alice` or the owner's daemon:

```bash
export S=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2 ALICE_HOME=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2/data-1m-prefill
[ -e "$ALICE_HOME/models" ] || ln -s "$HOME/.alice/models" "$ALICE_HOME/models"
nice -n 15 cargo build --release -j 6 -p alice-memory
ALICE_EMBEDDING_IDLE_UNLOAD=30s nice -n 15 target/release/alice daemon run & PID=$!
until target/release/alice status >/dev/null 2>&1; do sleep 1; done
target/release/alice recall "release checklist" >/dev/null
ps -o rss= -p $PID   # loaded
sleep 60
ps -o rss= -p $PID   # after idle unload
target/release/alice daemon stop
pgrep -fl "alice daemon run" || true
```

Add "RSS loaded / after idle unload" to `docs/benchmarks/m2.md`. (The first start migrates this data to schema v2 and writes a pre-migration copy of several GB under `$S/data-1m-prefill/backups/`; Task 18 deletes `$S/data-*`.)

- [ ] **Step 8: Commit**

```bash
nice -n 15 cargo fmt --all && nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
git add crates docs/benchmarks/m2.md
git commit -m "feat: live settings API, alice config and write-only secrets

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 7: Secret redaction on every write path

Spec §5.3: built-in detectors, `[REDACTED:<type>]` replacement, `block` mode that refuses the write naming the detector, custom patterns and an allowlist, applied to memories, raw chunks, Judge requests (Task 9), sync exports (M4) and logs, before anything touches disk (§5.1 step 2). This task builds the detector set in `alice-core::redact`, a cache of compiled redactors per project in the daemon, applies it to `remember`, `update` (edit), `ingest`, both spool paths (client and the daemon's read-only path), and proves with a byte scan that a planted secret reaches neither `alice.db`, the spool nor `daemon.log`.

**Files:**
- Create: `crates/alice-core/src/redact.rs`; modify `crates/alice-core/src/lib.rs`
- Create: `crates/alice-daemon/src/redaction.rs` (per-project redactor cache); modify `crates/alice-daemon/src/lib.rs`
- Modify: `crates/alice-daemon/src/service.rs` (`redact_remember`, redaction in `remember`, `update`, `ingest`), `crates/alice-daemon/src/http.rs` (spool the redacted request), `crates/alice-daemon/src/client.rs` (redact before spooling), `crates/alice-core/src/store/projects.rs` (`root_of`)
- Test: `redact.rs` unit corpus, service tests, `crates/alice/tests/cli.rs` (byte scan)

**Interfaces:**
- Consumes: `alice_core::settings::{PrivacySettings, RedactMode, CustomPattern, ProjectSettings, load_project}`, `Service::settings()`.
- Produces:
  - `alice_core::redact::{Redactor, Redaction, entropy}`: `Redactor::{new(p: &PrivacySettings) -> Result<Self>, builtin() -> Self, apply(&self, text: &str) -> Redaction, check(&self, field: &str, text: &str) -> Result<String>}`; `Redaction { text: String, found: Vec<String> }`. Detector names: `pem_private_key`, `jwt`, `aws_access_key`, `aws_secret_key`, `gcp_key_id`, `github_token`, `gitlab_token`, `slack_token`, `stripe_key`, `anthropic_key`, `openai_key`, `url_credentials`, `password_assignment`, `high_entropy`, `custom:<name>`.
  - `alice_daemon::redaction::Redactors::{new() -> Self, for_project(&self, global: &PrivacySettings, root: Option<&Path>) -> Result<Arc<Redactor>>, privacy_for(&self, global: &PrivacySettings, root: Option<&Path>) -> PrivacySettings}`
  - `Service::{redact_remember(&self, req: RememberRequest) -> Result<RememberRequest>, project_root(&self, choice: &ProjectChoice) -> Option<PathBuf>}`
  - `alice_core::store::projects::root_of(conn: &Connection, project_id_or_slug: &str) -> Result<Option<String>>`

- [ ] **Step 1: Read the files this task modifies, as they are now.**

- [ ] **Step 2: Write the failing redaction corpus**

`crates/alice-core/src/redact.rs`, test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{CustomPattern, PrivacySettings, RedactMode};

    /// (input, detector expected to fire, text that must be gone)
    const SECRETS: &[(&str, &str, &str)] = &[
        (concat!("key AKIA", "IOSFODNN7EXAMPLE in env"), "aws_access_key", concat!("AKIA", "IOSFODNN7EXAMPLE")),
        ("aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", "aws_secret_key", "wJalrXUtnFEMI/K7MDENG"),
        ("{\"private_key_id\": \"1234567890abcdef1234567890abcdef12345678\"}", "gcp_key_id", "1234567890abcdef1234567890abcdef12345678"),
        (concat!("push with gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789"), "github_token", concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789")),
        (concat!("github_", "pat_11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz"), "github_token", concat!("github_", "pat_11ABCDEFG")),
        (concat!("gl", "pat-abcdefghij0123456789xyz"), "gitlab_token", concat!("gl", "pat-abcdefghij0123456789")),
        (concat!("slack xo", "xb-123456789012-abcdefghijkl"), "slack_token", concat!("xo", "xb-123456789012")),
        (concat!("stripe sk_", "live_51HabcdefghijklmnopqrSTU"), "stripe_key", concat!("sk_", "live_51Habcdef")),
        (concat!("ANTHROPIC sk-", "ant-api03-abcdefghijklmnopqrstuvwxyz012345"), "anthropic_key", concat!("sk-", "ant-api03-abcdef")),
        ("openai sk-proj-abcdefghijklmnopqrstuvwxyz0123", "openai_key", "sk-proj-abcdefghijkl"),
        ("jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U", "jwt", "eyJhbGciOiJIUzI1NiJ9"),
        (concat!("-----BEGIN RSA PRIVATE ", "KEY-----\nMIIEpAIBAAKCAQEA1234\n-----END RSA PRIVATE KEY-----"), "pem_private_key", "MIIEpAIBAAKCAQEA1234"),
        ("db postgres://admin:s3cr3tPass@db.internal:5432/app", "url_credentials", "s3cr3tPass"),
        ("password = hunter2", "password_assignment", "hunter2"),
        ("API_KEY: \"correcthorsebatterystaple\"", "password_assignment", "correcthorsebatterystaple"),
        ("รหัสผ่าน: abc12345 สำหรับเครื่องทดสอบ", "password_assignment", "abc12345"),
        ("STRIPE_SIGNING_KEY=whsec_9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f0a", "high_entropy", "whsec_9f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f0a"),
    ];

    /// Text that must come out unchanged (code and prose that only look like secrets).
    const SAFE: &[&str] = &[
        "let token = parse_token(input);",
        "password: required",
        "token: String,",
        "api_key = os.environ[\"API_KEY\"]",
        "secret = config.secret_value",
        "set the TOKEN env var to your-token-here",
        "the sk-learn library and sketching",
        "http://example.com/path@section",
        "ใช้ WAL mode เพื่อให้ reader ไม่บล็อก writer",
        "already [REDACTED:github_token] here",
    ];

    #[test]
    fn every_builtin_detector_fires_and_removes_the_secret() {
        let r = Redactor::builtin();
        for (input, detector, secret) in SECRETS {
            let out = r.apply(input);
            assert!(out.found.contains(&detector.to_string()), "{input:?}: found {:?}, text {:?}", out.found, out.text);
            assert!(!out.text.contains(secret), "{input:?} → {:?}", out.text);
            assert!(out.text.contains(&format!("[REDACTED:{detector}]")), "{:?}", out.text);
        }
    }

    #[test]
    fn code_and_prose_are_left_alone_and_redaction_is_idempotent() {
        let r = Redactor::builtin();
        for s in SAFE {
            let out = r.apply(s);
            assert_eq!(out.text, *s, "{s:?} changed ({:?})", out.found);
        }
        for (input, _, _) in SECRETS {
            let once = r.apply(input).text;
            assert_eq!(r.apply(&once).text, once, "{input:?}");
        }
    }

    #[test]
    fn block_mode_names_the_detector_and_off_keeps_text() {
        let block = Redactor::new(&PrivacySettings { redact_mode: RedactMode::Block, ..Default::default() }).unwrap();
        let err = block.check("content", concat!("deploy with gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789")).unwrap_err().to_string();
        assert!(err.contains("github_token") && !err.contains("ghp_"), "{err}");
        assert_eq!(block.check("content", "nothing secret").unwrap(), "nothing secret");
        let off = Redactor::new(&PrivacySettings { redact: false, ..Default::default() }).unwrap();
        assert!(off.check("content", "password = hunter2").unwrap().contains("hunter2"));
    }

    #[test]
    fn custom_patterns_and_the_allowlist() {
        let p = PrivacySettings {
            custom_patterns: vec![CustomPattern { name: "ticket".into(), regex: r"TCK-[0-9]{4,}".into() }],
            allowlist: vec![concat!("AKIA", "IOSFODNN7EXAMPLE").into()],
            ..Default::default()
        };
        let r = Redactor::new(&p).unwrap();
        let out = r.apply(concat!("see TCK-12345 and AKIA", "IOSFODNN7EXAMPLE"));
        assert_eq!(out.text, concat!("see [REDACTED:custom:ticket] and AKIA", "IOSFODNN7EXAMPLE"));
        assert_eq!(out.found, vec!["custom:ticket".to_string()]);
    }
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-core redact::` — Expected: FAIL to compile.

- [ ] **Step 3: Implement `redact.rs`**

```rust
//! Secret detection and redaction (spec §5.3). Runs before anything is written or sent.
//! Matches become `[REDACTED:<detector>]`; `block` mode refuses the text instead, naming the
//! detector (never the secret). Already-redacted text is left as it is.

use crate::error::{AliceError, Result};
use crate::settings::{PrivacySettings, RedactMode};
use regex::Regex;

struct Detector {
    name: String,
    re: Regex,
    /// Capture group to replace (0 = the whole match).
    group: usize,
    /// Extra test on the replaced text (false = not a secret).
    check: fn(&str) -> bool,
}

pub struct Redactor {
    enabled: bool,
    mode: RedactMode,
    detectors: Vec<Detector>,
    allow: Vec<Regex>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    pub text: String,
    /// Detector names that fired, in order of first appearance.
    pub found: Vec<String>,
}

/// Shannon entropy in bits per character.
pub fn entropy(s: &str) -> f64 {
    let mut counts = std::collections::HashMap::new();
    let n = s.chars().count() as f64;
    for c in s.chars() {
        *counts.entry(c).or_insert(0usize) += 1;
    }
    counts.values().map(|&k| { let p = k as f64 / n; -p * p.log2() }).sum()
}

fn any(_: &str) -> bool {
    true
}

/// A value after `password =` etc. that is a secret, not code or prose: no call, index,
/// template or dotted path; not an ENV_NAME; and either letters with digits or long and varied.
fn secret_value(v: &str) -> bool {
    if v.starts_with("[REDACTED") || v.contains(['(', ')', '[', ']', '{', '}', '<', '>', '$']) {
        return false;
    }
    let dotted = v.contains('.') && v.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_alphanumeric() || c == '_'));
    let env_name = v.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if dotted || env_name {
        return false;
    }
    let letters = v.chars().any(|c| c.is_alphabetic());
    let digits = v.chars().any(|c| c.is_ascii_digit());
    (letters && digits) || (v.chars().count() >= 16 && entropy(v) >= 3.0)
}

fn high_entropy(v: &str) -> bool {
    !v.starts_with("[REDACTED") && entropy(v) >= 3.5
}

fn builtin_detectors() -> Vec<Detector> {
    let d = |name: &str, re: &str, group: usize, check: fn(&str) -> bool| Detector {
        name: name.into(),
        re: Regex::new(re).expect("built-in pattern"),
        group,
        check,
    };
    vec![
        d("pem_private_key", r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z ]*PRIVATE KEY-----|\z)", 0, any),
        d("jwt", r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}", 0, any),
        d("aws_access_key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", 0, any),
        d("aws_secret_key", r#"(?i)\baws_?secret_?(?:access_?)?key\b\s*[:=]\s*["']?([A-Za-z0-9/+=]{40})"#, 1, any),
        d("gcp_key_id", r#""private_key_id"\s*:\s*"([0-9a-f]{40})""#, 1, any),
        d("github_token", r"\b(?:(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,255}|github_pat_[A-Za-z0-9_]{22,255})\b", 0, any),
        d("gitlab_token", r"\bglpat-[A-Za-z0-9_\-]{20,}", 0, any),
        d("slack_token", r"\bxox[abprs]-[A-Za-z0-9-]{10,}", 0, any),
        d("stripe_key", r"\b(?:sk|rk)_live_[A-Za-z0-9]{16,}", 0, any),
        d("anthropic_key", r"\bsk-ant-[A-Za-z0-9_\-]{20,}", 0, any),
        d("openai_key", r"\bsk-(?:proj-|svcacct-)?[A-Za-z0-9_\-]{20,}", 0, any),
        d("url_credentials", r"\b[A-Za-z][A-Za-z0-9+.\-]*://[^\s/:@\[\]]+:([^\s/@\[\]]+)@", 1, any),
        d(
            "password_assignment",
            r#"(?i)(?:\b(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key|auth[_-]?token|client[_-]?secret)\b|รหัสผ่าน)\s*[:=]\s*["']?([^\s"',;]{6,})"#,
            1,
            secret_value,
        ),
        d(
            "high_entropy",
            r#"(?i)\b[A-Za-z_][A-Za-z0-9_]*(?:key|secret|token|pass|pwd|auth|credential)[A-Za-z0-9_]*\s*[:=]\s*["']?([A-Za-z0-9+/=_\-]{32,})"#,
            1,
            high_entropy,
        ),
    ]
}

impl Redactor {
    pub fn builtin() -> Self {
        Self::new(&PrivacySettings::default()).expect("built-in detectors compile")
    }

    pub fn new(p: &PrivacySettings) -> Result<Self> {
        let mut detectors = builtin_detectors();
        for c in &p.custom_patterns {
            let re = Regex::new(&c.regex).map_err(|e| AliceError::invalid(format!(
                "privacy.custom_patterns {}: {e}; fix the regular expression", c.name
            )))?;
            detectors.push(Detector { name: format!("custom:{}", c.name), re, group: 0, check: any });
        }
        let allow = p
            .allowlist
            .iter()
            .map(|a| Regex::new(&format!("^(?:{a})$")))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| AliceError::invalid(format!("privacy.allowlist: {e}")))?;
        Ok(Self { enabled: p.redact, mode: p.redact_mode, detectors, allow })
    }

    pub fn apply(&self, text: &str) -> Redaction {
        let mut out = text.to_string();
        let mut found: Vec<String> = Vec::new();
        if !self.enabled {
            return Redaction { text: out, found };
        }
        for d in &self.detectors {
            let tag = format!("[REDACTED:{}]", d.name);
            let mut result = String::with_capacity(out.len());
            let mut last = 0;
            let mut hit = false;
            for caps in d.re.captures_iter(&out) {
                let Some(m) = caps.get(d.group) else { continue };
                let value = m.as_str();
                if !(d.check)(value) || self.allow.iter().any(|a| a.is_match(value)) {
                    continue;
                }
                result.push_str(&out[last..m.start()]);
                result.push_str(&tag);
                last = m.end();
                hit = true;
            }
            if hit {
                result.push_str(&out[last..]);
                out = result;
                if !found.contains(&d.name) {
                    found.push(d.name.clone());
                }
            }
        }
        Redaction { text: out, found }
    }

    /// The text to store for `field`: redacted, or refused in `block` mode.
    pub fn check(&self, field: &str, text: &str) -> Result<String> {
        let r = self.apply(text);
        if self.mode == RedactMode::Block && !r.found.is_empty() {
            return Err(AliceError::invalid(format!(
                "the {field} contains what looks like a secret ({}); remove it and send again, or add an exception to privacy.allowlist",
                r.found.join(", ")
            )));
        }
        Ok(r.text)
    }
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-core redact::` — Expected: 4 passed. Tune a pattern only if a `SECRETS` case fails to fire or a `SAFE` case changes; never delete a case.

- [ ] **Step 4: Per-project redactors in the daemon**

`crates/alice-core/src/store/projects.rs`:

```rust
/// The working-tree root a project was detected from, if it has one.
pub fn root_of(conn: &Connection, project_id_or_slug: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT root_path FROM projects WHERE id = ?1 OR slug = ?1
             UNION ALL SELECT p.root_path FROM project_aliases a JOIN projects p ON p.id = a.project_id WHERE a.alias = ?1
             LIMIT 1",
            [project_id_or_slug],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}
```

`crates/alice-daemon/src/redaction.rs`:

```rust
//! Compiled redactors: one for the global privacy settings, one per project root whose
//! `.alice.toml` adds rules (re-read when the file changes). Compiling the detectors costs
//! milliseconds, so it is never done per request.

use alice_core::error::{AliceError, Result};
use alice_core::redact::Redactor;
use alice_core::settings::{PrivacySettings, load_project};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

type Entry = (Option<SystemTime>, PrivacySettings, Arc<Redactor>);

#[derive(Default)]
pub struct Redactors {
    global: Mutex<Option<(PrivacySettings, Arc<Redactor>)>>,
    projects: Mutex<HashMap<PathBuf, Entry>>,
}

fn modified(root: &Path) -> Option<SystemTime> {
    std::fs::metadata(root.join(".alice.toml")).and_then(|m| m.modified()).ok()
}

impl Redactors {
    pub fn new() -> Self {
        Self::default()
    }

    /// The effective privacy settings for a project root (global ones when there is none).
    pub fn privacy_for(&self, global: &PrivacySettings, root: Option<&Path>) -> PrivacySettings {
        match root.map(load_project) {
            Some(Ok(p)) => p.apply_to(global),
            Some(Err(e)) => {
                tracing::warn!(file = %e.key, message = %e.message, "ignoring an invalid project .alice.toml");
                global.clone()
            }
            None => global.clone(),
        }
    }

    pub fn for_project(&self, global: &PrivacySettings, root: Option<&Path>) -> Result<Arc<Redactor>> {
        if let Some(root) = root.filter(|r| r.join(".alice.toml").exists()) {
            let stamp = modified(root);
            let mut cache = self.projects.lock();
            if let Some((s, p, r)) = cache.get(root)
                && *s == stamp
                && *p == self.privacy_for(global, Some(root))
            {
                return Ok(r.clone());
            }
            let p = self.privacy_for(global, Some(root));
            let r = Arc::new(Redactor::new(&p)?);
            cache.insert(root.to_path_buf(), (stamp, p, r.clone()));
            return Ok(r);
        }
        let mut g = self.global.lock();
        if let Some((p, r)) = g.as_ref()
            && p == global
        {
            return Ok(r.clone());
        }
        let r = Arc::new(Redactor::new(global).map_err(|e| AliceError::Other(format!("privacy settings: {e}")))?);
        *g = Some((global.clone(), r.clone()));
        Ok(r)
    }
}
```

- [ ] **Step 5: Write the failing service tests**

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn secrets_are_redacted_before_they_are_stored_and_block_mode_refuses() {
        let (d, s) = svc().await;
        let cwd = repo(&d, "proj");
        let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        let r = s.remember(RememberRequest { content: format!("deploy key is {token}"), cwd: cwd.clone(), ..Default::default() }).await.unwrap();
        let got = s.get(GetRequest { ids: vec![r.id.clone().unwrap()], include_provenance: false }).await.unwrap();
        let body = &got.memories[0].memory.body;
        assert!(body.contains("[REDACTED:github_token]") && !body.contains(token), "{body}");
        s.ingest(IngestRequest {
            source: SourceInput { uri: "s".into(), ..Default::default() },
            chunks: vec![ChunkInput { ord: 0, text: format!("export GITHUB_TOKEN={token}"), ..Default::default() }],
            cwd: cwd.clone(), ..Default::default()
        }).await.unwrap();
        let edited = s.update(UpdateRequest { id: r.id.clone().unwrap(), update: MemoryUpdate {
            action: UpdateAction::Edit, body: Some("db postgres://u:s3cr3tPass@h/db".into()), ..Default::default() }, ..Default::default() }).await.unwrap();
        assert!(!edited.memory.body.contains("s3cr3tPass"));
        // Per project: .alice.toml with block_secrets refuses instead.
        std::fs::write(std::path::Path::new(cwd.as_deref().unwrap()).join(".alice.toml"), "[privacy]\nblock_secrets = true\n").unwrap();
        let err = s.remember(RememberRequest { content: format!("again {token}"), cwd: cwd.clone(), ..Default::default() }).await.unwrap_err();
        assert!(err.to_string().contains("github_token"), "{err}");
        // Raw bytes of the database (and its WAL) never held the token.
        s.shutdown();
        for f in ["alice.db", "alice.db-wal"] {
            if let Ok(bytes) = std::fs::read(d.path().join(f)) {
                assert!(!bytes.windows(token.len()).any(|w| w == token.as_bytes()), "{f} contains the token");
            }
        }
    }
```

Run: `nice -n 15 cargo test -j 6 -p alice-daemon secrets_are_redacted` — Expected: FAIL (token stored).

- [ ] **Step 6: Apply redaction in the service, the HTTP spool path and the client spool path**

`Service` gets `redactors: Redactors`. Add:

```rust
    /// The working-tree root of the project a write goes to, if known.
    pub fn project_root(&self, choice: &ProjectChoice) -> Option<PathBuf> {
        match choice {
            ProjectChoice::Detected(r) => r.root.as_ref().map(PathBuf::from),
            ProjectChoice::Slug(s) => self
                .readers
                .with(|c| projects::root_of(c, s))
                .ok()
                .flatten()
                .map(PathBuf::from),
        }
    }

    fn redactor(&self, choice: &ProjectChoice) -> Result<Arc<alice_core::redact::Redactor>> {
        let root = self.project_root(choice);
        self.redactors.for_project(&self.settings().privacy, root.as_deref())
    }

    /// `req` with secrets redacted (or an error in block mode). Idempotent.
    pub async fn redact_remember(&self, mut req: RememberRequest) -> Result<RememberRequest> {
        let choice = self.choose(req.project.as_deref(), req.cwd.as_deref()).await?;
        let r = self.redactor(&choice)?;
        req.content = r.check("content", &req.content)?;
        if let Some(t) = &req.title {
            req.title = Some(r.check("title", t)?);
        }
        Ok(req)
    }
```

(If `ProjectRef.root` is named differently, use its field for the repository root; `project_root` runs on the blocking pool when called from async code — wrap the `Slug` branch in `spawn_blocking` if it shows up in latency.) In `remember`, redact `input.content`/`input.title` with `self.redactor(&project)?` right after `choose`. In `update` (Edit with `title`/`body`), resolve the memory's project slug with a read (`SELECT p.slug FROM memories m JOIN projects p ON p.id = m.project_id WHERE m.id = ?1 OR m.legacy_id = ?1`) and redact with `ProjectChoice::Slug(slug)`. In `ingest`, redact every chunk's `text` (and `source.uri` through `url_credentials` only via the same redactor) before `validate_chunks`.

`http.rs` `remember`: call `let req = st.svc.redact_remember(req).await?;` first, so the read-only spool path writes the redacted request.

`client.rs`: before `spool_entry` in `remember_via` and `ingest_or_spool`, redact with the global settings read from disk (the daemon is down, so its settings are not available):

```rust
/// Privacy settings from config.toml and the environment, for writes spooled while the daemon
/// is away (the daemon applies project rules again when it drains the spool).
fn spool_redactor(home: &AliceHome) -> Result<alice_core::redact::Redactor> {
    // Task 5's one rule for "config.toml + ALICE_* + defaults" outside the daemon.
    let privacy = crate::config::load_effective(home).resolved.settings.privacy;
    alice_core::redact::Redactor::new(&privacy)
}
```

and in `remember_via`, after `check_remember(&req)?`: `let r = spool_redactor(home)?; req.content = r.check("content", &req.content)?; if let Some(t) = &req.title { req.title = Some(r.check("title", t)?); }` (make `req` `mut`); likewise every chunk text in `ingest_or_spool`.

Client test (`client.rs` tests or `crates/alice-daemon/tests/daemon_lifecycle.rs`): with no daemon and `Launcher::None`, `remember_or_spool` of a text holding a GitHub token returns `Spooled` and the spool file's bytes do not contain the token.

Run: `nice -n 15 cargo test -j 6 --workspace` — Expected: pass.

- [ ] **Step 7: End-to-end byte scan (process test, unix)**

In `crates/alice/tests/cli.rs`:

```rust
#[test]
fn a_planted_secret_reaches_neither_the_database_the_spool_nor_the_log() {
    let env = Env::new();
    let token = concat!("gh", "p_PlantedSecretToken0123456789abcdefABCD");
    env.ok(&["remember", &format!("ci uses {token} for releases")]);
    env.ok(&["daemon", "stop"]);
    // While the daemon is down the CLI spools; the spool file must not hold the token either.
    let out = env.cmd(&["remember", &format!("second {token}")]).env("ALICE_NO_AUTOSTART", "1").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut files = vec![env.home.join("logs/daemon.log")];
    for dir in ["", "spool"] {
        for e in std::fs::read_dir(env.home.join(dir)).unwrap().flatten() {
            if e.path().is_file() {
                files.push(e.path());
            }
        }
    }
    for f in files {
        let bytes = std::fs::read(&f).unwrap_or_default();
        assert!(!bytes.windows(token.len()).any(|w| w == token.as_bytes()), "{} holds the token", f.display());
    }
}
```

If the CLI has no switch to stop auto-start, add `ALICE_NO_AUTOSTART=1` handling to `launcher()` in `cmd.rs` (returns `Launcher::None`), documented in the README's environment table. Run: `nice -n 15 cargo test -j 6 -p alice-memory --test cli a_planted_secret` — Expected: pass; then the full suite and `pgrep -fl "alice daemon run"`.

- [ ] **Step 8: Commit**

```bash
nice -n 15 cargo fmt --all && nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
git add crates README.md README.th.md
git commit -m "feat: secret redaction before anything is stored or spooled; block mode and per-project rules

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 8: Judge core — trait, verbalisation, heuristic provider, cache and cost store

Spec §5.2: a provider-neutral layer for decisions with calibrated probabilities (`classify`, `score`, `compare`); the `heuristic` provider is the offline default. This task defines the trait and its types in `alice-core::judge`, the task helpers every provider gets (kind, project routing, predicate matching, relevance grading), the verbalisation Jev needs ("two weeks ago", "importance high"), the heuristic provider, and the `judge_cache` / `cost_ledger` store functions Task 9 uses.

**Files:**
- Create: `crates/alice-core/src/judge/mod.rs`, `judge/verbal.rs`, `judge/text.rs`, `judge/heuristic.rs`
- Create: `crates/alice-core/src/store/judge.rs`
- Modify: `crates/alice-core/src/lib.rs` (`pub mod judge;`), `crates/alice-core/src/store/mod.rs` (`pub mod judge;`), `crates/alice-core/Cargo.toml` (`strsim`), root `Cargo.toml` (`strsim = "0.11.1"`)

**Interfaces:**
- Consumes: `alice_core::model::Kind`, `store::kg::normalize_predicate`, `normalize::nfc`.
- Produces:
  - `alice_core::judge::{RelationKind, Choice, Distribution, Rubric, Score, JudgeMemory, Relation, ProjectOption, JudgeError, JudgeResult, Judge, kind_choices, relation_choices, reverse_relation_choices, relevance_choices, RELEVANCE_ANSWERS, RELEVANCE_PARTIAL}`
  - `RelationKind::{Duplicate, Updates, Contradicts, Unrelated}` with `as_str`, `parse`, `ALL`.
  - `Distribution::{from_pairs(impl IntoIterator<Item = (String, f64)>) -> Self, certain(id: &str) -> Self, uniform(ids: &[&str]) -> Self, p(&self, id: &str) -> f64, best(&self) -> Option<(String, f64)>, average(a: &Self, b: &Self) -> Self}` (normalised; `best` breaks ties by id order, so it is deterministic)
  - `trait Judge: Send + Sync { fn provider(&self) -> &str; fn classify(&self, state: &str, options: &[Choice]) -> JudgeResult<Distribution>; fn score(&self, state: &str, rubric: &Rubric) -> JudgeResult<Score>; fn compare(&self, new: &JudgeMemory, existing: &[JudgeMemory]) -> JudgeResult<Vec<Relation>>; fn score_many(&self, items: &[(String, Rubric)]) -> JudgeResult<Vec<Score>>; fn classify_kind(&self, m: &JudgeMemory) -> JudgeResult<Distribution>; fn route_project(&self, m: &JudgeMemory, projects: &[ProjectOption]) -> JudgeResult<Distribution>; fn match_predicate(&self, new: &str, existing: &[String]) -> JudgeResult<Distribution>; fn grade_relevance(&self, query: &str, results: &[JudgeMemory]) -> JudgeResult<Vec<Distribution>> }` (the last five have default implementations built on `classify` / `score`)
  - `alice_core::judge::verbal::{number(n: u64) -> String, age(created_at: &str, now: DateTime<Utc>) -> String, importance(i: u8) -> &'static str, memory_block(label: &str, m: &JudgeMemory, now: DateTime<Utc>) -> String, BODY_CHARS: usize = 700}`
  - `alice_core::judge::text::{normalize_title(&str) -> String, title_similarity(&str, &str) -> f64, stem_predicate(&str) -> String, predicate_similarity(&str, &str) -> f64}`
  - `alice_core::judge::heuristic::{HeuristicJudge { duplicate_similarity: f32, title_similarity: f64 }, kind_rules(title: &str, body: &str) -> Distribution}` with `Default` (0.97, 0.9), provider name `"heuristic"`.
  - `alice_core::store::judge::{cache_get(conn, hash) -> Result<Option<String>>, cache_put(conn, hash, provider, model, response_json) -> Result<()>, spent(conn, day, provider) -> Result<f64>, add_cost(conn, &CostDelta) -> Result<()>, cost_since(conn, day) -> Result<Vec<CostRow>>, CostDelta { day, provider, calls, input_tokens, output_tokens, usd }, CostRow (same fields)}`

- [ ] **Step 1: Read the files this task modifies, as they are now.**

- [ ] **Step 2: Write the failing tests**

`crates/alice-core/src/judge/mod.rs` tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distributions_normalise_and_pick_deterministically() {
        let d = Distribution::from_pairs([("a".to_string(), 2.0), ("b".to_string(), 2.0), ("c".to_string(), -1.0)]);
        assert!((d.p("a") - 0.5).abs() < 1e-9 && d.p("c") == 0.0 && d.p("zzz") == 0.0);
        assert_eq!(d.best(), Some(("a".to_string(), 0.5)), "ties go to the first id");
        let avg = Distribution::average(&Distribution::certain("a"), &Distribution::certain("b"));
        assert_eq!((avg.p("a"), avg.p("b")), (0.5, 0.5));
        assert_eq!(Distribution::uniform(&["x", "y", "z", "w"]).p("w"), 0.25);
        assert_eq!(Distribution::from_pairs(Vec::<(String, f64)>::new()).best(), None);
    }

    #[test]
    fn relation_kinds_round_trip_and_choices_are_described() {
        for k in RelationKind::ALL {
            assert_eq!(RelationKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(relation_choices().len(), 4);
        assert!(reverse_relation_choices().iter().any(|c| c.id == "outdated_by"));
        assert_eq!(kind_choices().len(), 11);
        assert!(kind_choices().iter().all(|c| !c.description.is_empty()));
    }
}
```

`judge/verbal.rs` tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn ages_and_numbers_are_words() {
        let now = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let at = |d: i64| (now - chrono::Duration::days(d)).to_rfc3339();
        assert_eq!(age(&at(0), now), "today");
        assert_eq!(age(&at(1), now), "yesterday");
        assert_eq!(age(&at(3), now), "three days ago");
        assert_eq!(age(&at(8), now), "a week ago");
        assert_eq!(age(&at(15), now), "two weeks ago");
        assert_eq!(age(&at(40), now), "a month ago");
        assert_eq!(age(&at(200), now), "six months ago");
        assert_eq!(age(&at(400), now), "a year ago");
        assert_eq!(age(&at(1_100), now), "three years ago");
        assert_eq!(age("not a date", now), "at an unknown time");
        assert_eq!(number(12), "twelve");
        assert_eq!(number(40), "forty");
        assert_eq!(number(137), "137");
        assert_eq!(importance(4), "importance high");
    }

    #[test]
    fn memory_blocks_have_no_digits_from_metadata_and_bound_the_body() {
        let now = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let m = JudgeMemory {
            id: "01X".into(), project: "app".into(), kind: crate::model::Kind::Decision,
            title: "Use WAL".into(), body: "x".repeat(5_000), importance: 5,
            created_at: (now - chrono::Duration::days(14)).to_rfc3339(), similarity: None,
        };
        let b = memory_block("Existing memory 1", &m, now);
        assert!(b.starts_with("Existing memory 1 (written two weeks ago, kind decision, importance critical, project app):"), "{b}");
        assert!(b.chars().count() < BODY_CHARS + 200);
    }
}
```

`judge/heuristic.rs` tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::{Judge, JudgeMemory, ProjectOption};
    use crate::model::Kind;

    fn mem(id: &str, project: &str, kind: Kind, title: &str, created: &str, sim: Option<f32>) -> JudgeMemory {
        JudgeMemory { id: id.into(), project: project.into(), kind, title: title.into(), body: title.into(),
            importance: 3, created_at: created.into(), similarity: sim }
    }

    #[test]
    fn compare_finds_duplicates_in_the_project_and_suggests_updates() {
        let j = HeuristicJudge::default();
        let new = mem("N", "app", Kind::Decision, "Use WAL mode for the hub DB", "2026-10-13T00:00:00.000Z", None);
        let out = j.compare(&new, &[
            mem("D", "app", Kind::Decision, "Use WAL mode", "2026-10-01T00:00:00.000Z", Some(0.985)),
            mem("X", "other", Kind::Decision, "Use WAL mode", "2026-10-01T00:00:00.000Z", Some(0.99)),
            mem("U", "app", Kind::Decision, "Use WAL mode for the hub database", "2026-09-01T00:00:00.000Z", Some(0.90)),
            mem("Z", "app", Kind::Fact, "Tom yum recipe", "2026-09-01T00:00:00.000Z", Some(0.40)),
        ]).unwrap();
        let by = |id: &str| out.iter().find(|r| r.existing_id == id).unwrap();
        assert_eq!(by("D").probs.best().unwrap().0, "duplicate");
        assert!(by("D").probs.p("duplicate") >= 0.97);
        assert_eq!(by("X").probs.best().unwrap().0, "unrelated", "never across projects");
        assert_eq!(by("U").probs.best().unwrap().0, "updates");
        assert!(by("U").probs.p("updates") < 0.85, "a heuristic update is only ever a suggestion");
        assert_eq!(by("Z").probs.best().unwrap().0, "unrelated");
    }

    #[test]
    fn kind_rules_read_english_and_thai_cues() {
        assert_eq!(kind_rules("Fixed the crash", "Root cause: a null pointer. Fix: check first.").best().unwrap().0, "bugfix");
        assert_eq!(kind_rules("ตัดสินใจเลือกใช้ SQLite", "เพราะไม่ต้องมี server").best().unwrap().0, "decision");
        assert_eq!(kind_rules("Never run migrations on prod by hand", "careful: it locks tables").best().unwrap().0, "warning");
        let vague = kind_rules("Meeting", "we talked");
        assert!(vague.best().unwrap().1 < 0.5);
    }

    #[test]
    fn routing_and_predicates() {
        let j = HeuristicJudge::default();
        let m = mem("N", "global", Kind::Note, "pg-ai v4 frames must be framed protobuf", "2026-10-13T00:00:00.000Z", None);
        let projects = vec![
            ProjectOption { slug: "pg-ai".into(), name: "pg-ai".into(), hint: String::new() },
            ProjectOption { slug: "alice".into(), name: "alice".into(), hint: String::new() },
        ];
        let d = j.route_project(&m, &projects).unwrap();
        assert_eq!(d.best().unwrap().0, "pg-ai");
        assert!(d.best().unwrap().1 < 0.85);
        let p = j.match_predicate("uses", &["use".into(), "deployed_on".into()]).unwrap();
        assert_eq!(p.best().unwrap(), ("use".to_string(), 0.95));
        let none = j.match_predicate("owns", &["deployed_on".into()]).unwrap();
        assert_eq!(none.best().unwrap().0, "none");
    }
}
```

`crates/alice-core/src/store/judge.rs` tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_and_costs() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        assert_eq!(cache_get(&c, "h").unwrap(), None);
        cache_put(&c, "h", "jev", "jev-1.13.0", "{\"answers\":{}}").unwrap();
        cache_put(&c, "h", "jev", "jev-1.13.0", "{\"answers\":{\"q\":1}}").unwrap();
        assert_eq!(cache_get(&c, "h").unwrap().as_deref(), Some("{\"answers\":{\"q\":1}}"));
        for _ in 0..3 {
            add_cost(&c, &CostDelta { day: "2026-10-13".into(), provider: "jev".into(), calls: 1, input_tokens: 400, output_tokens: 0, usd: 0.0000168 }).unwrap();
        }
        assert!((spent(&c, "2026-10-13", "jev").unwrap() - 0.0000504).abs() < 1e-12);
        assert_eq!(spent(&c, "2026-10-14", "jev").unwrap(), 0.0);
        let rows = cost_since(&c, "2026-10-01").unwrap();
        assert_eq!((rows.len(), rows[0].calls, rows[0].input_tokens), (1, 3, 1_200));
    }
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-core judge:: store::judge::` — Expected: FAIL to compile.

- [ ] **Step 3: Implement `judge/mod.rs`**

```rust
//! The Judge (spec §5.2): decisions as probabilities over named choices, from a provider that
//! may be offline (`heuristic`) or a model (`jev`). Callers apply a decision automatically at
//! `judge.auto_apply_threshold`, ask in the review inbox at `judge.review_threshold`, and
//! otherwise do nothing; a failing judge never fails a write.

pub mod heuristic;
pub mod text;
pub mod verbal;

use crate::model::Kind;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    Duplicate,
    Updates,
    Contradicts,
    Unrelated,
}

impl RelationKind {
    pub const ALL: [RelationKind; 4] = [Self::Duplicate, Self::Updates, Self::Contradicts, Self::Unrelated];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate",
            Self::Updates => "updates",
            Self::Contradicts => "contradicts",
            Self::Unrelated => "unrelated",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub id: String,
    pub description: String,
}

impl Choice {
    pub fn new(id: &str, description: &str) -> Self {
        Self { id: id.into(), description: description.into() }
    }
}

/// Probabilities over choice ids, normalised to sum to 1 (empty when nothing had mass).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Distribution(pub BTreeMap<String, f64>);

impl Distribution {
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, f64)>) -> Self {
        let mut m: BTreeMap<String, f64> = BTreeMap::new();
        for (k, v) in pairs {
            *m.entry(k).or_default() += if v.is_finite() { v.max(0.0) } else { 0.0 };
        }
        let total: f64 = m.values().sum();
        if total > 0.0 {
            m.values_mut().for_each(|v| *v /= total);
        } else {
            m.clear();
        }
        Self(m)
    }
    pub fn certain(id: &str) -> Self {
        Self::from_pairs([(id.to_string(), 1.0)])
    }
    pub fn uniform(ids: &[&str]) -> Self {
        Self::from_pairs(ids.iter().map(|i| (i.to_string(), 1.0)))
    }
    pub fn p(&self, id: &str) -> f64 {
        self.0.get(id).copied().unwrap_or(0.0)
    }
    pub fn best(&self) -> Option<(String, f64)> {
        self.0
            .iter()
            .fold(None, |acc: Option<(&String, f64)>, (k, &v)| match acc {
                Some((_, b)) if b >= v => acc,
                _ => Some((k, v)),
            })
            .map(|(k, v)| (k.clone(), v))
    }
    pub fn average(a: &Self, b: &Self) -> Self {
        Self::from_pairs(a.0.iter().chain(b.0.iter()).map(|(k, v)| (k.clone(), v / 2.0)))
    }
}

/// Levels of a graded answer, worst first; a score is the expected level in 0..=1.
#[derive(Debug, Clone, PartialEq)]
pub struct Rubric {
    pub question: String,
    pub levels: Vec<Choice>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Score {
    pub value: f64,
    pub confidence: f64,
}

/// A memory as a judge sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeMemory {
    pub id: String,
    /// Project slug.
    pub project: String,
    pub kind: Kind,
    pub title: String,
    pub body: String,
    pub importance: u8,
    pub created_at: String,
    /// Cosine similarity to the memory it is compared with, when known.
    pub similarity: Option<f32>,
}

/// How a new memory relates to one existing memory: P over `RelationKind` names.
#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    pub existing_id: String,
    pub probs: Distribution,
    /// Forward and reversed questions picked the same answer (providers that ask both).
    pub agreement: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectOption {
    pub slug: String,
    pub name: String,
    /// A few recent titles or topics, to tell projects apart.
    pub hint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JudgeError {
    #[error("the judge's daily budget is used up ({0})")]
    Budget(String),
    #[error("this project may not be sent to a cloud judge")]
    Denied,
    #[error("the judge is unavailable: {0}")]
    Unavailable(String),
    #[error("the judge failed: {0}")]
    Failed(String),
}

pub type JudgeResult<T> = std::result::Result<T, JudgeError>;

pub const RELEVANCE_ANSWERS: &str = "answers";
pub const RELEVANCE_PARTIAL: &str = "partially";

pub fn relation_choices() -> Vec<Choice> {
    vec![
        Choice::new("duplicate", "both say the same thing; neither adds anything"),
        Choice::new("updates", "the new memory replaces the existing one with newer or corrected information about the same thing"),
        Choice::new("contradicts", "they cannot both be true, and the new one does not clearly replace the existing one"),
        Choice::new("unrelated", "different subjects, or compatible information that should both be kept"),
    ]
}

/// The same question asked from the existing memory's side (`outdated_by` maps to `updates`).
pub fn reverse_relation_choices() -> Vec<Choice> {
    vec![
        Choice::new("duplicate", "both say the same thing; neither adds anything"),
        Choice::new("outdated_by", "the existing memory is replaced by the new one: newer or corrected information about the same thing"),
        Choice::new("contradicts", "they cannot both be true, and the new one does not clearly replace the existing one"),
        Choice::new("unrelated", "different subjects, or compatible information that should both be kept"),
    ]
}

pub fn kind_choices() -> Vec<Choice> {
    vec![
        Choice::new("fact", "a fact about code, a system or the world"),
        Choice::new("decision", "a decision that was made, ideally with its reason"),
        Choice::new("bugfix", "a bug: its symptom, cause and fix"),
        Choice::new("howto", "steps or commands that achieve something"),
        Choice::new("preference", "how the user wants things done"),
        Choice::new("warning", "a danger, or something to avoid"),
        Choice::new("status", "the current state of some work"),
        Choice::new("note", "anything else worth keeping"),
        Choice::new("summary", "a summary of a longer discussion or document"),
        Choice::new("diary", "a journal entry about what happened in a session"),
        Choice::new("handoff", "what is done, what is next and open questions, for the next session"),
    ]
}

pub fn relevance_choices() -> Vec<Choice> {
    vec![
        Choice::new(RELEVANCE_ANSWERS, "the result answers the query"),
        Choice::new(RELEVANCE_PARTIAL, "the result is about the query's subject but does not answer it"),
        Choice::new("unrelated", "the result is not about the query"),
    ]
}

pub trait Judge: Send + Sync {
    fn provider(&self) -> &str;

    /// P over `options` for `state`.
    fn classify(&self, state: &str, options: &[Choice]) -> JudgeResult<Distribution>;

    /// The expected level of `rubric` (0 = worst, 1 = best) and how sure the judge is.
    fn score(&self, state: &str, rubric: &Rubric) -> JudgeResult<Score>;

    /// Per existing memory: P(duplicate | updates | contradicts | unrelated).
    fn compare(&self, new: &JudgeMemory, existing: &[JudgeMemory]) -> JudgeResult<Vec<Relation>>;

    fn score_many(&self, items: &[(String, Rubric)]) -> JudgeResult<Vec<Score>> {
        items.iter().map(|(s, r)| self.score(s, r)).collect()
    }

    fn classify_kind(&self, m: &JudgeMemory) -> JudgeResult<Distribution> {
        let now = chrono::Utc::now();
        self.classify(&verbal::kind_state(m, now), &kind_choices())
    }

    fn route_project(&self, m: &JudgeMemory, projects: &[ProjectOption]) -> JudgeResult<Distribution> {
        let now = chrono::Utc::now();
        let options: Vec<Choice> = projects
            .iter()
            .map(|p| Choice::new(&p.slug, &format!("{}: {}", p.name, p.hint)))
            .chain([Choice::new("global", "not about any one project")])
            .collect();
        self.classify(&verbal::memory_block("Memory", m, now), &options)
    }

    fn match_predicate(&self, new: &str, existing: &[String]) -> JudgeResult<Distribution> {
        let options: Vec<Choice> = existing
            .iter()
            .map(|p| Choice::new(p, &p.replace('_', " ")))
            .chain([Choice::new("none", "none of these means the same")])
            .collect();
        self.classify(&format!("A knowledge-graph relation is named \"{}\". Which existing relation name means the same?", new.replace('_', " ")), &options)
    }

    fn grade_relevance(&self, query: &str, results: &[JudgeMemory]) -> JudgeResult<Vec<Distribution>> {
        let now = chrono::Utc::now();
        results
            .iter()
            .map(|r| self.classify(&format!("Query: {query}\n\n{}", verbal::memory_block("Result", r, now)), &relevance_choices()))
            .collect()
    }
}
```

- [ ] **Step 4: Implement `verbal.rs`, `text.rs`, `heuristic.rs`**

`judge/verbal.rs`:

```rust
//! Numbers and dates in words: Jev reads "two weeks ago" and "importance high" better than
//! digits. Only metadata is verbalised; memory text is sent as written.

use super::JudgeMemory;
use chrono::{DateTime, Utc};

/// Characters of a memory body sent to a judge.
pub const BODY_CHARS: usize = 700;

const WORDS: [&str; 21] = [
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen",
    "nineteen", "twenty",
];
const TENS: [&str; 10] = ["", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"];

pub fn number(n: u64) -> String {
    match n {
        0..=20 => WORDS[n as usize].into(),
        _ if n < 100 && n % 10 == 0 => TENS[(n / 10) as usize].into(),
        _ => n.to_string(),
    }
}

fn ago(n: i64, one: &str, unit: &str) -> String {
    if n <= 1 { format!("{one} {unit} ago") } else { format!("{} {unit}s ago", number(n as u64)) }
}

pub fn age(created_at: &str, now: DateTime<Utc>) -> String {
    let Ok(t) = DateTime::parse_from_rfc3339(created_at) else {
        return "at an unknown time".into();
    };
    let days = (now - t.with_timezone(&Utc)).num_days();
    match days {
        ..=0 => "today".into(),
        1 => "yesterday".into(),
        2..=6 => format!("{} days ago", number(days as u64)),
        7..=13 => "a week ago".into(),
        14..=29 => ago(days / 7, "a", "week"),
        30..=59 => "a month ago".into(),
        60..=364 => ago(days / 30, "a", "month"),
        365..=729 => "a year ago".into(),
        _ => ago(days / 365, "a", "year"),
    }
}

pub fn importance(i: u8) -> &'static str {
    match i {
        0 | 1 => "importance minor",
        2 => "importance low",
        3 => "importance normal",
        4 => "importance high",
        _ => "importance critical",
    }
}

pub fn memory_block(label: &str, m: &JudgeMemory, now: DateTime<Utc>) -> String {
    let body = crate::normalize::truncate_chars(&m.body, BODY_CHARS);
    format!(
        "{label} (written {}, kind {}, {}, project {}):\nTitle: {}\n{}\n",
        age(&m.created_at, now),
        m.kind.as_str(),
        importance(m.importance),
        m.project,
        crate::normalize::truncate_chars(&m.title, 200),
        body
    )
}

pub fn kind_state(m: &JudgeMemory, now: DateTime<Utc>) -> String {
    format!("Which kind of memory is this?\n\n{}", memory_block("Memory", m, now))
}
```

(Make the tests' expected words match: 15 days → `14..=29` → `ago(2, …)` = "two weeks ago"; 200 days → `200/30 = 6` → "six months ago"; 1,100 days → "three years ago".)

`judge/text.rs`:

```rust
//! Text comparisons for the heuristic judge.

use crate::normalize::nfc;
use crate::store::kg::normalize_predicate;

pub fn normalize_title(s: &str) -> String {
    let lower = nfc(s).to_lowercase();
    let cleaned: String = lower.chars().map(|c| if c.is_alphanumeric() || unicode_normalization::char::is_combining_mark(c) { c } else { ' ' }).collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn title_similarity(a: &str, b: &str) -> f64 {
    strsim::jaro_winkler(&normalize_title(a), &normalize_title(b))
}

/// Snake-case predicate with English inflections removed per word: `uses` → `use`,
/// `deployed_on` → `deploy_on`, `running_on` → `run_on`.
pub fn stem_predicate(p: &str) -> String {
    normalize_predicate(p)
        .split('_')
        .map(|w| {
            let n = w.chars().count();
            if n > 5 && w.ends_with("ing") {
                let base = &w[..w.len() - 3];
                let b = base.as_bytes();
                if b.len() >= 2 && b[b.len() - 1] == b[b.len() - 2] { base[..base.len() - 1].to_string() } else { base.to_string() }
            } else if n > 4 && w.ends_with("ied") {
                format!("{}y", &w[..w.len() - 3])
            } else if n > 4 && w.ends_with("ed") {
                w[..w.len() - 2].to_string()
            } else if n > 3 && w.ends_with("es") && !w.ends_with("ses") {
                w[..w.len() - 1].to_string()
            } else if n > 3 && w.ends_with('s') && !w.ends_with("ss") {
                w[..w.len() - 1].to_string()
            } else {
                w.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("_")
}

pub fn predicate_similarity(a: &str, b: &str) -> f64 {
    strsim::jaro_winkler(&stem_predicate(a), &stem_predicate(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_and_similarities() {
        assert_eq!(stem_predicate("uses"), "use");
        assert_eq!(stem_predicate("Deployed on"), "deploy_on");
        assert_eq!(stem_predicate("running_on"), "run_on");
        assert_eq!(stem_predicate("ใช้งาน"), "ใช้งาน");
        assert!(title_similarity("Use WAL mode!", "use wal MODE") > 0.99);
        assert!(predicate_similarity("uses", "use") > 0.99);
        assert!(predicate_similarity("owns", "deployed_on") < 0.9);
    }
}
```

`judge/heuristic.rs`:

```rust
//! The offline judge (spec §5.2): duplicates by cosine similarity in the same project,
//! "updates" suggestions by kind and near-identical titles, kinds by keyword rules, projects by
//! name mentions, predicates by string similarity. It never claims more certainty than the
//! rules justify: only duplicates (and identical predicates) reach the auto-apply threshold.

use super::text::{predicate_similarity, stem_predicate, title_similarity};
use super::{Choice, Distribution, Judge, JudgeMemory, JudgeResult, ProjectOption, Relation, Rubric, Score};

pub struct HeuristicJudge {
    pub duplicate_similarity: f32,
    pub title_similarity: f64,
}

impl Default for HeuristicJudge {
    fn default() -> Self {
        Self { duplicate_similarity: 0.97, title_similarity: 0.9 }
    }
}

/// Keyword cues per kind (lowercase; English and Thai).
const CUES: &[(&str, &[&str])] = &[
    ("bugfix", &["fix", "bug", "crash", "error", "regression", "root cause", "symptom", "แก้", "บั๊ก", "พัง"]),
    ("decision", &["decided", "decision", "we will", "chose", "choose", "instead of", "going with", "ตัดสินใจ", "เลือกใช้"]),
    ("howto", &["how to", "steps", "run `", "command", "วิธี", "ขั้นตอน"]),
    ("preference", &["prefer", "i like", "always use", "please", "ชอบ", "อยากให้"]),
    ("warning", &["warning", "careful", "never ", "do not", "don't", "danger", "avoid", "ระวัง", "ห้าม"]),
    ("status", &["status", "in progress", "currently", "blocked", "สถานะ", "กำลังทำ"]),
    ("handoff", &["handoff", "next steps", "open questions", "ส่งต่อ", "งานต่อไป"]),
    ("summary", &["summary", "in short", "tl;dr", "สรุป"]),
    ("diary", &["today i", "this session", "diary", "บันทึก"]),
    ("fact", &[" is a ", "runs on", " uses ", "located", "version"]),
];

/// P over kinds from keyword cues: 0.8 for a clear winner with two or more cues, 0.6 with one,
/// spread over the tied kinds otherwise; `note` when nothing matched.
pub fn kind_rules(title: &str, body: &str) -> Distribution {
    let text = format!(" {} \n {} ", title.to_lowercase(), body.to_lowercase());
    let mut hits: Vec<(&str, usize)> = CUES
        .iter()
        .map(|(k, cues)| (*k, cues.iter().filter(|c| text.contains(*c)).count()))
        .filter(|(_, n)| *n > 0)
        .collect();
    hits.sort_by(|a, b| b.1.cmp(&a.1));
    match hits.as_slice() {
        [] => spread("note", 0.4),
        [(k, n), rest @ ..] if rest.first().is_none_or(|(_, m)| m < n) => spread(k, if *n >= 2 { 0.8 } else { 0.6 }),
        _ => {
            let top = hits[0].1;
            Distribution::from_pairs(
                hits.iter()
                    .filter(|(_, n)| *n == top)
                    .map(|(k, _)| (k.to_string(), 1.0))
                    .chain([("note".to_string(), 0.5)]),
            )
        }
    }
}

/// `p` on `winner`, the rest shared by the other kinds.
fn spread(winner: &str, p: f64) -> Distribution {
    let kinds: Vec<String> = super::kind_choices().into_iter().map(|c| c.id).collect();
    let share = (1.0 - p) / (kinds.len() - 1) as f64;
    Distribution::from_pairs(kinds.into_iter().map(|k| {
        let v = if k == winner { p } else { share };
        (k, v)
    }))
}

fn mentions(text: &str, name: &str) -> bool {
    let text = text.to_lowercase();
    let name = name.to_lowercase();
    [name.clone(), name.replace('-', " "), name.replace('-', "_")].iter().any(|n| {
        text.match_indices(n.as_str()).any(|(i, _)| {
            let before = text[..i].chars().next_back();
            let after = text[i + n.len()..].chars().next();
            before.is_none_or(|c| !c.is_alphanumeric()) && after.is_none_or(|c| !c.is_alphanumeric())
        })
    })
}

impl Judge for HeuristicJudge {
    fn provider(&self) -> &str {
        "heuristic"
    }

    /// The heuristic cannot read arbitrary questions: no information.
    fn classify(&self, _state: &str, options: &[Choice]) -> JudgeResult<Distribution> {
        Ok(Distribution::from_pairs(options.iter().map(|c| (c.id.clone(), 1.0))))
    }

    fn score(&self, _state: &str, _rubric: &Rubric) -> JudgeResult<Score> {
        Ok(Score { value: 0.5, confidence: 0.0 })
    }

    fn compare(&self, new: &JudgeMemory, existing: &[JudgeMemory]) -> JudgeResult<Vec<Relation>> {
        Ok(existing
            .iter()
            .map(|e| {
                let same_project = e.project == new.project;
                let sim = e.similarity.unwrap_or(0.0);
                let probs = if same_project && sim >= self.duplicate_similarity {
                    let p = f64::from(sim).min(0.99);
                    Distribution::from_pairs([("duplicate".to_string(), p), ("unrelated".to_string(), 1.0 - p)])
                } else if same_project
                    && e.kind == new.kind
                    && new.created_at > e.created_at
                    && title_similarity(&new.title, &e.title) >= self.title_similarity
                {
                    Distribution::from_pairs([("updates".to_string(), 0.7), ("unrelated".to_string(), 0.3)])
                } else {
                    Distribution::certain("unrelated")
                };
                Relation { existing_id: e.id.clone(), probs, agreement: None }
            })
            .collect())
    }

    fn classify_kind(&self, m: &JudgeMemory) -> JudgeResult<Distribution> {
        Ok(kind_rules(&m.title, &m.body))
    }

    fn route_project(&self, m: &JudgeMemory, projects: &[ProjectOption]) -> JudgeResult<Distribution> {
        let text = format!("{}\n{}", m.title, m.body);
        let hits: Vec<&ProjectOption> = projects.iter().filter(|p| mentions(&text, &p.slug) || mentions(&text, &p.name)).collect();
        Ok(match hits.as_slice() {
            [one] => Distribution::from_pairs([(one.slug.clone(), 0.7), ("global".to_string(), 0.3)]),
            _ => Distribution::certain("global"),
        })
    }

    fn match_predicate(&self, new: &str, existing: &[String]) -> JudgeResult<Distribution> {
        let stem = stem_predicate(new);
        let best = existing
            .iter()
            .map(|p| (p, predicate_similarity(new, p)))
            .max_by(|a, b| a.1.total_cmp(&b.1));
        Ok(match best {
            Some((p, _)) if stem_predicate(p) == stem => Distribution::from_pairs([(p.clone(), 0.95), ("none".to_string(), 0.05)]),
            Some((p, s)) if s >= self.title_similarity => Distribution::from_pairs([(p.clone(), 0.7), ("none".to_string(), 0.3)]),
            _ => Distribution::certain("none"),
        })
    }
}
```

- [ ] **Step 5: Implement `store/judge.rs`**

```rust
//! Judge cache (by request hash) and the per-day cost ledger (spec §3.3, §5.2).

use crate::error::Result;
use crate::ids::now_rfc3339;
use rusqlite::{Connection, OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CostDelta {
    /// UTC day, `YYYY-MM-DD`.
    pub day: String,
    pub provider: String,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub usd: f64,
}

pub type CostRow = CostDelta;

pub fn cache_get(conn: &Connection, hash: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT response_json FROM judge_cache WHERE request_hash = ?1", [hash], |r| r.get(0))
        .optional()?)
}

pub fn cache_put(conn: &Connection, hash: &str, provider: &str, model: &str, response_json: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO judge_cache (request_hash, provider, model, response_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(request_hash) DO UPDATE SET response_json = excluded.response_json, created_at = excluded.created_at",
        params![hash, provider, model, response_json, now_rfc3339()],
    )?;
    Ok(())
}

pub fn spent(conn: &Connection, day: &str, provider: &str) -> Result<f64> {
    Ok(conn
        .query_row("SELECT usd FROM cost_ledger WHERE day = ?1 AND provider = ?2", params![day, provider], |r| r.get(0))
        .optional()?
        .unwrap_or(0.0))
}

pub fn add_cost(conn: &Connection, d: &CostDelta) -> Result<()> {
    conn.execute(
        "INSERT INTO cost_ledger (day, provider, calls, input_tokens, output_tokens, usd) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(day, provider) DO UPDATE SET calls = calls + excluded.calls,
           input_tokens = input_tokens + excluded.input_tokens,
           output_tokens = output_tokens + excluded.output_tokens, usd = usd + excluded.usd",
        params![d.day, d.provider, d.calls as i64, d.input_tokens as i64, d.output_tokens as i64, d.usd],
    )?;
    Ok(())
}

pub fn cost_since(conn: &Connection, day: &str) -> Result<Vec<CostRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT day, provider, calls, input_tokens, output_tokens, usd FROM cost_ledger WHERE day >= ?1 ORDER BY day DESC, provider",
    )?;
    Ok(stmt
        .query_map([day], |r| Ok(CostDelta {
            day: r.get(0)?, provider: r.get(1)?, calls: r.get::<_, i64>(2)? as u64,
            input_tokens: r.get::<_, i64>(3)? as u64, output_tokens: r.get::<_, i64>(4)? as u64, usd: r.get(5)?,
        }))?
        .collect::<std::result::Result<_, _>>()?)
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-core judge:: store::judge::` — Expected: all pass.

- [ ] **Step 6: Commit**

```bash
nice -n 15 cargo fmt --all && nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
git add Cargo.toml Cargo.lock crates/alice-core
git commit -m "feat: judge trait, verbalisation, heuristic provider, judge cache and cost ledger

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 9: Jev provider — requests, cache, budget, rate limit, retries, fixtures, live smoke test

Spec §5.2 (Jev): numbers verbalised, options shuffled per call, forward and reversed questions in the same request averaged for decisions that may supersede, responses cached by SHA-256 of the request, retries only on 429/5xx/timeouts, ≤ 10 requests/s, a daily USD budget with a hard stop and every call in `cost_ledger`, never sending `privacy.cloud_denied_projects`, redaction first. The provider logic is pure and lives in `alice-core::judge::jev` behind two small traits (transport, store), so unit tests replay recorded fixtures with no network; the daemon supplies a reqwest transport and a store that reads through the reader pool and writes through the writer.

**Files:**
- Create: `crates/alice-core/src/judge/jev.rs` (+ `pub mod jev;` and the default `Judge::warning` in `judge/mod.rs`)
- Create: `crates/alice-core/tests/fixtures/jev/{compare.json,kind.json,relevance.json,classify.json}` (recorded in Step 8)
- Create: `crates/alice-embed/src/net.rs` (`BlockingHttp`), modify `crates/alice-embed/src/lib.rs`
- Create: `crates/alice-daemon/src/judge_http.rs`; modify `crates/alice-daemon/src/lib.rs`, `writer.rs` (`WriteOp::JudgeCall`), `service.rs` (judge construction and rebuild, `test_provider("judge")`)
- Create: `crates/alice-daemon/tests/jev_live.rs` (ignored: smoke test and fixture recorder)

**Interfaces:**
- Consumes: Task 8 (`Judge`, `Choice`, `Distribution`, `JudgeMemory`, `Relation`, `ProjectOption`, `JudgeError`, choices, `verbal`), Task 7 (`Redactor`), Task 5 (`JudgeSettings`, `PrivacySettings`, `SecretName::JevApiKey`), Task 6 (`Service::settings`, `test_provider`).
- Produces:
  - `alice_core::judge::jev::{JevJudge, JevConfig, JevTransport, HttpReply, TransportError, JevStore, JevCall, MemoryJevStore, JevStats, Question, Clock, Sleeper, USD_PER_MTOK_INPUT, scenarios}`
  - `JevConfig { endpoint, model, api_key, rate_per_sec: f64, timeout: Duration, daily_budget_usd: f64, max_retries: u32, cloud_denied: Vec<String> }`, `JevConfig::from_settings(&JudgeSettings, &PrivacySettings, api_key: String) -> Self`
  - `JevJudge::{new(cfg, transport: Arc<dyn JevTransport>, store: Arc<dyn JevStore>, redactor: Arc<Redactor>) -> Self, with_clock(self, Clock) -> Self, with_sleeper(self, Sleeper) -> Self, stats(&self) -> JevStats, ask(&self, state: &str, questions: &[Question]) -> JudgeResult<BTreeMap<String, Distribution>>, request_body(&self, state: &str, questions: &[Question]) -> Vec<u8>}`; `impl Judge for JevJudge` (provider `"jev"`).
  - `JevTransport::post(&self, endpoint: &str, key: &str, body: &[u8], timeout: Duration) -> Result<HttpReply, TransportError>`; `HttpReply { status: u16, body: Vec<u8>, retry_after: Option<Duration> }`; `TransportError::{Timeout, Connect(String)}`
  - `JevStore::{cached(&self, hash: &str) -> Option<String>, spent(&self, day: &str) -> Result<f64, String>, record(&self, call: &JevCall)}` (an unreadable spend fails closed: `JudgeError::Budget`); `JevCall { day, hash, model, response_json, input_tokens, output_tokens, usd, latency_ms }`
  - `alice_embed::net::{BlockingHttp, HttpResponse, HttpError}`: `BlockingHttp::{new(user_agent: &str) -> std::io::Result<Self>, post_json(&self, url: &str, bearer: Option<&str>, body: Vec<u8>, timeout: Duration) -> Result<HttpResponse, HttpError>}`
  - `alice_daemon::judge_http::{ReqwestTransport, DaemonJevStore, build_judge(settings: &Settings, secrets: &dyn SecretStore, readers: Arc<ReaderPool>, writer: WriterHandle, transport: Option<Arc<dyn JevTransport>>) -> (Arc<dyn Judge>, Option<String>)}` (the string is a warning such as "judge.provider is jev but no TypeSafe key is set; using the heuristic")
  - `WriteOp::JudgeCall(JevCall)`; `Judge::warning(&self) -> Option<String>` (a default method added to Task 8's trait, `None`; `JevJudge` returns its spend-read failure); `Service::{judge(&self) -> Arc<dyn Judge>, judge_warning(&self) -> Option<String>}` (the build warning, else `judge().warning()`); `ServiceOptions::jev_transport: Option<Arc<dyn JevTransport>>` (tests inject a scripted transport).

- [ ] **Step 1: Read the files this task modifies, as they are now.**

- [ ] **Step 2: `BlockingHttp`**

`crates/alice-embed/src/net.rs`:

```rust
//! Blocking HTTP usable from any thread — worker threads, `spawn_blocking`, plain tests — and
//! never on an async runtime thread's own executor: requests run on a private one-thread
//! runtime and the caller waits on a std channel. (Starting or blocking on a runtime from
//! inside another runtime panics; this does neither.)

use std::time::Duration;

pub struct BlockingHttp {
    rt: Option<tokio::runtime::Runtime>,
    client: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    /// Lowercase header names.
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    Timeout,
    Connect(String),
    Other(String),
}

impl BlockingHttp {
    pub fn new(user_agent: &str) -> std::io::Result<Self> {
        crate::install_tls_provider();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("alice-http")
            .enable_all()
            .build()?;
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(Self { rt: Some(rt), client })
    }

    pub fn post_json(&self, url: &str, bearer: Option<&str>, body: Vec<u8>, timeout: Duration) -> Result<HttpResponse, HttpError> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut req = self
            .client
            .post(url)
            .timeout(timeout)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(k) = bearer {
            req = req.bearer_auth(k);
        }
        self.rt.as_ref().expect("runtime lives until drop").spawn(async move {
            let r = async {
                let resp = req.send().await?;
                let status = resp.status().as_u16();
                let headers = resp
                    .headers()
                    .iter()
                    .map(|(k, v)| (k.as_str().to_ascii_lowercase(), v.to_str().unwrap_or("").to_string()))
                    .collect();
                let body = resp.bytes().await?.to_vec();
                Ok::<_, reqwest::Error>(HttpResponse { status, body, headers })
            }
            .await;
            let _ = tx.send(r);
        });
        match rx.recv_timeout(timeout + Duration::from_secs(5)) {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) if e.is_timeout() => Err(HttpError::Timeout),
            Ok(Err(e)) if e.is_connect() => Err(HttpError::Connect(e.to_string())),
            Ok(Err(e)) => Err(HttpError::Other(e.to_string())),
            Err(_) => Err(HttpError::Timeout),
        }
    }
}

impl Drop for BlockingHttp {
    /// `shutdown_background` is allowed inside an async context, unlike dropping a runtime.
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}
```

Test in `net.rs` (reuse `lib.rs`'s minimal HTTP server helper by moving it to a `#[cfg(test)] pub(crate) mod test_server` if needed): from inside `#[tokio::test(flavor = "multi_thread")]`, call `post_json` via `tokio::task::spawn_blocking` against the test server (200 with a body) and against a stalled route with a 300 ms timeout (expect `HttpError::Timeout`); then drop the `BlockingHttp` inside the async test without a panic.

- [ ] **Step 3: Write the failing Jev unit tests (no network)**

`crates/alice-core/src/judge/jev.rs` tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::{Judge, JudgeError, RelationKind};
    use parking_lot::Mutex as PMutex;
    use std::collections::VecDeque;

    /// Replays fixtures by request; anything else is a test failure that names the request.
    struct Fixtures(Vec<(serde_json::Value, serde_json::Value)>);

    impl Fixtures {
        fn load() -> Self {
            let mut v = Vec::new();
            for raw in [
                include_str!("../../tests/fixtures/jev/compare.json"),
                include_str!("../../tests/fixtures/jev/kind.json"),
                include_str!("../../tests/fixtures/jev/relevance.json"),
                include_str!("../../tests/fixtures/jev/classify.json"),
            ] {
                let f: serde_json::Value = serde_json::from_str(raw).unwrap();
                v.push((f["request"].clone(), f["response"].clone()));
            }
            Self(v)
        }
    }

    impl JevTransport for Fixtures {
        fn post(&self, _e: &str, key: &str, body: &[u8], _t: Duration) -> Result<HttpReply, TransportError> {
            assert_eq!(key, "test-key");
            let req: serde_json::Value = serde_json::from_slice(body).unwrap();
            let (_, resp) = self.0.iter().find(|(r, _)| *r == req).unwrap_or_else(|| {
                panic!("no fixture for this request; re-record with jev_live::record_fixtures:\n{req:#}")
            });
            Ok(HttpReply { status: 200, body: serde_json::to_vec(resp).unwrap(), retry_after: None })
        }
    }

    /// Answers from a script: status codes or transport errors, then 200 with `ok`.
    struct Scripted {
        script: PMutex<VecDeque<Result<u16, TransportError>>>,
        ok: serde_json::Value,
        calls: PMutex<Vec<serde_json::Value>>,
    }

    impl JevTransport for Scripted {
        fn post(&self, _e: &str, _k: &str, body: &[u8], _t: Duration) -> Result<HttpReply, TransportError> {
            self.calls.lock().push(serde_json::from_slice(body).unwrap());
            match self.script.lock().pop_front() {
                Some(Ok(status)) => Ok(HttpReply { status, body: b"{\"detail\":\"x\"}".to_vec(), retry_after: Some(Duration::ZERO) }),
                Some(Err(e)) => Err(e),
                None => Ok(HttpReply { status: 200, body: serde_json::to_vec(&self.ok).unwrap(), retry_after: None }),
            }
        }
    }

    fn cfg() -> JevConfig {
        JevConfig {
            endpoint: "https://api.typesafe.ai/v1/systemone".into(),
            model: "jev-1.13.0".into(),
            api_key: "test-key".into(),
            rate_per_sec: 10.0,
            timeout: Duration::from_secs(10),
            daily_budget_usd: 1.0,
            max_retries: 3,
            cloud_denied: vec!["secret-proj".into()],
        }
    }

    fn judge(t: Arc<dyn JevTransport>, store: Arc<MemoryJevStore>, cfg: JevConfig) -> JevJudge {
        JevJudge::new(cfg, t, store, Arc::new(Redactor::builtin()))
            .with_clock(Arc::new(|| scenarios::fixed_now()))
            .with_sleeper(Arc::new(|_| {}))
    }

    fn classify_ok() -> serde_json::Value {
        classify_ok_tokens(100)
    }

    /// A yes/no answer whose usage reports `input_tokens`.
    fn classify_ok_tokens(input_tokens: u64) -> serde_json::Value {
        serde_json::json!({"model": "jev-1.13.0", "answers": {"q": {"type": "choice", "choice": "yes", "confidence": 0.9,
            "probabilities": {"yes": 0.9, "no": 0.1}}}, "usage": {"input_tokens": input_tokens, "output_tokens": 3}})
    }

    fn yes_no() -> Vec<Choice> {
        vec![Choice::new("yes", "it is"), Choice::new("no", "it is not")]
    }

    #[test]
    fn the_request_body_has_the_documented_shape_and_no_key() {
        let j = judge(Arc::new(Fixtures::load()), Arc::new(MemoryJevStore::default()), cfg());
        let body = j.request_body("state text", &[Question { id: "q".into(), instructions: "Pick one.".into(), options: yes_no() }]);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["model"], "jev-1.13.0");
        assert_eq!(v["state"], "state text");
        assert_eq!(v["questions"]["q"]["type"], "choice");
        assert!(v["questions"]["q"]["criteria"].is_object(), "criteria must be an object");
        assert!(!String::from_utf8_lossy(&body).contains("test-key"));
        assert_eq!(body, j.request_body("state text", &[Question { id: "q".into(), instructions: "Pick one.".into(), options: yes_no() }]), "deterministic");
    }

    #[test]
    fn options_are_shuffled_but_the_same_question_always_gets_the_same_order() {
        let j = judge(Arc::new(Fixtures::load()), Arc::new(MemoryJevStore::default()), cfg());
        let opts: Vec<Choice> = (0..6).map(|i| Choice::new(&format!("o{i}"), "x")).collect();
        let mut orders = std::collections::HashSet::new();
        for i in 0..20 {
            let body = j.request_body(&format!("state {i}"), &[Question { id: "q".into(), instructions: "Pick.".into(), options: opts.clone() }]);
            let text = String::from_utf8(body).unwrap();
            let pos: Vec<usize> = (0..6).map(|k| text.find(&format!("\"o{k}\"")).unwrap()).collect();
            let mut order: Vec<usize> = (0..6).collect();
            order.sort_by_key(|&k| pos[k]);
            orders.insert(order);
        }
        assert!(orders.len() > 3, "options are shuffled across requests: {} orders", orders.len());
    }

    #[test]
    fn compare_averages_forward_and_reversed_from_the_recorded_fixture() {
        let store = Arc::new(MemoryJevStore::default());
        let j = judge(Arc::new(Fixtures::load()), store.clone(), cfg());
        let (new, existing) = scenarios::compare_case();
        let rel = j.compare(&new, &existing).unwrap();
        assert_eq!(rel.len(), existing.len());
        for r in &rel {
            let total: f64 = RelationKind::ALL.iter().map(|k| r.probs.p(k.as_str())).sum();
            assert!((total - 1.0).abs() < 1e-6, "{r:?}");
            assert!(r.agreement.is_some());
        }
        assert_eq!(rel[0].probs.best().unwrap().0, scenarios::COMPARE_EXPECTED[0]);
        assert_eq!(store.calls().len(), 1, "one request for all pairs, both directions");
        assert!(store.calls()[0].usd > 0.0);
        // The same decision again: answered from the cache, no request, no cost.
        let again = j.compare(&new, &existing).unwrap();
        assert_eq!(again, rel);
        assert_eq!(store.calls().len(), 1);
        assert_eq!(j.stats().cache_hits, 1);
    }

    #[test]
    fn kind_and_relevance_fixtures_parse() {
        let j = judge(Arc::new(Fixtures::load()), Arc::new(MemoryJevStore::default()), cfg());
        let m = scenarios::kind_case();
        assert_eq!(j.classify_kind(&m).unwrap().best().unwrap().0, scenarios::KIND_EXPECTED);
        let (q, results) = scenarios::relevance_case();
        let grades = j.grade_relevance(&q, &results).unwrap();
        assert_eq!(grades.len(), results.len());
        assert!(grades[0].p(crate::judge::RELEVANCE_ANSWERS) > grades[2].p(crate::judge::RELEVANCE_ANSWERS));
    }

    #[test]
    fn retries_only_on_429_5xx_and_timeouts() {
        let s = Arc::new(Scripted { script: PMutex::new(VecDeque::from([Ok(429), Ok(503), Err(TransportError::Timeout)])), ok: classify_ok(), calls: PMutex::new(Vec::new()) });
        let j = judge(s.clone(), Arc::new(MemoryJevStore::default()), cfg());
        assert_eq!(j.classify("is this a test?", &yes_no()).unwrap().best().unwrap().0, "yes");
        assert_eq!((s.calls.lock().len(), j.stats().retries), (4, 3));
        for (status, want) in [(400u16, "400"), (401, "set-secret jev_api_key"), (422, "422")] {
            let s = Arc::new(Scripted { script: PMutex::new(VecDeque::from([Ok(status)])), ok: classify_ok(), calls: PMutex::new(Vec::new()) });
            let j = judge(s.clone(), Arc::new(MemoryJevStore::default()), cfg());
            let e = j.classify(&format!("state {status}"), &yes_no()).unwrap_err();
            assert!(e.to_string().contains(want), "{status}: {e}");
            assert_eq!(s.calls.lock().len(), 1, "{status} is not retried");
        }
        let s = Arc::new(Scripted { script: PMutex::new(VecDeque::from(vec![Err(TransportError::Timeout); 4])), ok: classify_ok(), calls: PMutex::new(Vec::new()) });
        let j = judge(s.clone(), Arc::new(MemoryJevStore::default()), cfg());
        assert!(matches!(j.classify("t", &yes_no()), Err(JudgeError::Unavailable(_))));
        assert_eq!(s.calls.lock().len(), 4, "one try and three retries");
        // Spec §5.2: a connection error is not retried; the heuristic decides and the memory's
        // judge_state becomes 'retry' (Task 12).
        let s = Arc::new(Scripted { script: PMutex::new(VecDeque::from([Err(TransportError::Connect("connection refused".into()))])), ok: classify_ok(), calls: PMutex::new(Vec::new()) });
        let j = judge(s.clone(), Arc::new(MemoryJevStore::default()), cfg());
        let e = j.classify("c", &yes_no()).unwrap_err();
        assert!(matches!(e, JudgeError::Unavailable(_)) && e.to_string().contains("cannot reach Jev"), "{e}");
        assert_eq!((s.calls.lock().len(), j.stats().retries), (1, 0), "connection errors are not retried");
    }

    /// A judge sharing `s` and `store`, with a movable clock and the given daily budget.
    fn budget_judge(s: &Arc<Scripted>, store: &Arc<MemoryJevStore>, day: &Arc<PMutex<DateTime<Utc>>>, budget: f64) -> JevJudge {
        let d = day.clone();
        JevJudge::new(JevConfig { daily_budget_usd: budget, ..cfg() }, s.clone(), store.clone(), Arc::new(Redactor::builtin()))
            .with_clock(Arc::new(move || *d.lock()))
            .with_sleeper(Arc::new(|_| {}))
    }

    /// What `classify(state, yes_no())` reserves: the body's bytes as tokens (ratio 1).
    fn estimate(j: &JevJudge, state: &str) -> f64 {
        let q = Question { id: "q".into(), instructions: "Choose the option that fits best.".into(), options: yes_no() };
        j.request_body(state, std::slice::from_ref(&q)).len() as f64 * USD_PER_MTOK_INPUT / 1e6
    }

    #[test]
    fn the_daily_budget_is_never_exceeded_and_resets_the_next_utc_day() {
        let store = Arc::new(MemoryJevStore::default());
        let s = Arc::new(Scripted { script: PMutex::new(VecDeque::new()), ok: classify_ok(), calls: PMutex::new(Vec::new()) });
        let day = Arc::new(PMutex::new(scenarios::fixed_now()));
        // Budget: exactly the first request's estimate. The reply reports 100 input tokens,
        // fewer than the body's bytes, so the first call fits and settles below its estimate.
        let budget = estimate(&budget_judge(&s, &store, &day, 1.0), "first");
        let j = budget_judge(&s, &store, &day, budget);
        j.classify("first", &yes_no()).unwrap();
        let e = j.classify("second", &yes_no()).unwrap_err();
        assert!(matches!(e, JudgeError::Budget(_)), "{e}");
        assert_eq!(s.calls.lock().len(), 1, "no request once the next estimate does not fit");
        assert!(store.spent("2026-10-13").unwrap() <= budget, "the day's spend never exceeds the budget");
        *day.lock() = scenarios::fixed_now() + chrono::Duration::days(1);
        j.classify("third", &yes_no()).unwrap();
        assert_eq!(s.calls.lock().len(), 2);
    }

    #[test]
    fn a_reply_counting_more_tokens_than_bytes_raises_later_estimates() {
        let store = Arc::new(MemoryJevStore::default());
        let s = Arc::new(Scripted { script: PMutex::new(VecDeque::new()), ok: classify_ok_tokens(10_000), calls: PMutex::new(Vec::new()) });
        let day = Arc::new(PMutex::new(scenarios::fixed_now()));
        let probe = budget_judge(&s, &store, &day, 1.0);
        // Enough for the first call as Jev counts it, not for a second one of the same size.
        let budget = estimate(&probe, "first") + 10_000.0 * USD_PER_MTOK_INPUT / 1e6;
        let j = budget_judge(&s, &store, &day, budget);
        j.classify("first", &yes_no()).unwrap();
        // "2nd" is shorter than "first": by bytes alone it would fit and overspend.
        assert!(matches!(j.classify("2nd", &yes_no()), Err(JudgeError::Budget(_))));
        assert!(store.spent("2026-10-13").unwrap() <= budget);
    }

    #[test]
    fn an_unreadable_spend_counts_as_an_exhausted_budget() {
        struct BrokenSpend;
        impl JevStore for BrokenSpend {
            fn cached(&self, _: &str) -> Option<String> { None }
            fn spent(&self, _: &str) -> Result<f64, String> { Err("disk I/O error".into()) }
            fn record(&self, _: &JevCall) {}
        }
        let s = Arc::new(Scripted { script: PMutex::new(VecDeque::new()), ok: classify_ok(), calls: PMutex::new(Vec::new()) });
        let j = JevJudge::new(cfg(), s.clone(), Arc::new(BrokenSpend), Arc::new(Redactor::builtin()))
            .with_clock(Arc::new(|| scenarios::fixed_now()))
            .with_sleeper(Arc::new(|_| {}));
        assert!(matches!(j.classify("x", &yes_no()), Err(JudgeError::Budget(_))));
        assert!(s.calls.lock().is_empty(), "fail closed: nothing is sent");
        assert!(j.warning().unwrap().contains("cannot read today's Jev spend"), "{:?}", j.warning());
    }

    #[test]
    fn denied_projects_never_leave_and_text_is_redacted_first() {
        let s = Arc::new(Scripted { script: PMutex::new(VecDeque::new()), ok: classify_ok(), calls: PMutex::new(Vec::new()) });
        let j = judge(s.clone(), Arc::new(MemoryJevStore::default()), cfg());
        let (mut new, existing) = scenarios::compare_case();
        new.project = "secret-proj".into();
        assert!(matches!(j.compare(&new, &existing), Err(JudgeError::Denied)));
        let mut m = scenarios::kind_case();
        m.project = "secret-proj".into();
        assert!(matches!(j.classify_kind(&m), Err(JudgeError::Denied)));
        assert!(s.calls.lock().is_empty());
        j.classify(concat!("token gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789 here"), &yes_no()).unwrap();
        let sent = s.calls.lock()[0].to_string();
        assert!(sent.contains("[REDACTED:github_token]") && !sent.contains("ghp_"), "{sent}");
    }

    #[test]
    fn requests_are_spaced_to_the_rate_limit() {
        let waited = Arc::new(PMutex::new(Duration::ZERO));
        let w = waited.clone();
        let s = Arc::new(Scripted { script: PMutex::new(VecDeque::new()), ok: classify_ok(), calls: PMutex::new(Vec::new()) });
        let j = JevJudge::new(cfg(), s, Arc::new(MemoryJevStore::default()), Arc::new(Redactor::builtin()))
            .with_clock(Arc::new(|| scenarios::fixed_now()))
            .with_sleeper(Arc::new(move |d| {
                let mut m = w.lock();
                *m = (*m).max(d);
            }));
        for i in 0..20 {
            j.classify(&format!("distinct state {i}"), &yes_no()).unwrap();
        }
        // The sleeper does not really sleep, so the 20th call's slot is ~1.9 s after the first.
        let longest = *waited.lock();
        assert!(longest >= Duration::from_millis(1_700) && longest <= Duration::from_millis(1_950), "{longest:?}");
    }

    #[test]
    fn malformed_or_incomplete_answers_fail_without_retry() {
        for body in [serde_json::json!("not an object"), serde_json::json!({"answers": {}, "usage": {"input_tokens": 1}})] {
            let s = Arc::new(Scripted { script: PMutex::new(VecDeque::new()), ok: body, calls: PMutex::new(Vec::new()) });
            let j = judge(s.clone(), Arc::new(MemoryJevStore::default()), cfg());
            assert!(matches!(j.classify("x", &yes_no()), Err(JudgeError::Failed(_))));
            assert_eq!(s.calls.lock().len(), 1);
        }
    }
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-core judge::jev` — Expected: FAIL to compile.

- [ ] **Step 4: Implement `jev.rs`**

```rust
//! TypeSafe Jev (spec §5.2): choice questions about one state, answered with probabilities.
//!
//! Every request: the state is redacted first; metadata is in words (`verbal`); each question's
//! options are shuffled with a seed derived from the request itself (a repeated decision builds
//! the same body, so the SHA-256 cache answers it; different decisions see different orders);
//! decisions that may supersede ask forward and reversed questions in one request and average
//! them. Calls are spaced to `rate_per_sec`, retried only on 429, 5xx and timeouts, and refused
//! once the UTC day's spend (reserved before the call, settled after) would pass the budget.

use super::verbal::{kind_state, memory_block};
use super::{
    Choice, Distribution, Judge, JudgeError, JudgeMemory, JudgeResult, ProjectOption, Relation, Rubric, Score,
    kind_choices, relation_choices, relevance_choices, reverse_relation_choices,
};
use crate::normalize::sha256_hex;
use crate::redact::Redactor;
use crate::settings::{JudgeSettings, PrivacySettings};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize, Serializer};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const USD_PER_MTOK_INPUT: f64 = 0.042;
const MAX_RETRY_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct HttpReply {
    pub status: u16,
    pub body: Vec<u8>,
    pub retry_after: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    Timeout,
    Connect(String),
}

pub trait JevTransport: Send + Sync {
    fn post(&self, endpoint: &str, key: &str, body: &[u8], timeout: Duration) -> Result<HttpReply, TransportError>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JevCall {
    pub day: String,
    pub hash: String,
    pub model: String,
    pub response_json: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub usd: f64,
    pub latency_ms: u64,
}

pub trait JevStore: Send + Sync {
    fn cached(&self, hash: &str) -> Option<String>;
    /// USD spent on `day`; `Err` when it cannot be read (the judge then fails closed).
    fn spent(&self, day: &str) -> Result<f64, String>;
    fn record(&self, call: &JevCall);
}

/// In-memory store for tests and the evaluation.
#[derive(Default)]
pub struct MemoryJevStore {
    cache: Mutex<BTreeMap<String, String>>,
    calls: Mutex<Vec<JevCall>>,
}

impl MemoryJevStore {
    pub fn calls(&self) -> Vec<JevCall> {
        self.calls.lock().clone()
    }
}

impl JevStore for MemoryJevStore {
    fn cached(&self, hash: &str) -> Option<String> {
        self.cache.lock().get(hash).cloned()
    }
    fn spent(&self, day: &str) -> Result<f64, String> {
        Ok(self.calls.lock().iter().filter(|c| c.day == day).map(|c| c.usd).sum())
    }
    fn record(&self, call: &JevCall) {
        self.cache.lock().insert(call.hash.clone(), call.response_json.clone());
        self.calls.lock().push(call.clone());
    }
}

#[derive(Debug, Clone)]
pub struct JevConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    pub rate_per_sec: f64,
    pub timeout: Duration,
    pub daily_budget_usd: f64,
    pub max_retries: u32,
    pub cloud_denied: Vec<String>,
}

impl JevConfig {
    pub fn from_settings(j: &JudgeSettings, p: &PrivacySettings, api_key: String) -> Self {
        Self {
            endpoint: j.jev.endpoint.clone(),
            model: j.jev.model.clone(),
            api_key,
            rate_per_sec: j.jev.rate_per_sec.clamp(0.1, 10.0),
            timeout: Duration::from_millis(j.timeout_ms),
            daily_budget_usd: j.daily_budget_usd,
            max_retries: 3,
            cloud_denied: p.cloud_denied_projects.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Question {
    pub id: String,
    pub instructions: String,
    pub options: Vec<Choice>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct JevStats {
    pub calls: u64,
    pub cache_hits: u64,
    pub retries: u64,
    pub failures: u64,
    pub budget_stops: u64,
    pub last_error: Option<String>,
    /// Latency of the last (at most 1,000) successful calls, in ms.
    pub latencies_ms: Vec<u64>,
}

pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;
pub type Sleeper = Arc<dyn Fn(Duration) + Send + Sync>;

struct Budget {
    day: String,
    spent: f64,
}

pub struct JevJudge {
    cfg: JevConfig,
    transport: Arc<dyn JevTransport>,
    store: Arc<dyn JevStore>,
    redactor: Arc<Redactor>,
    clock: Clock,
    sleep: Sleeper,
    next_slot: Mutex<Option<Instant>>,
    budget: Mutex<Option<Budget>>,
    /// Input tokens per request byte, at least 1 (raised if Jev ever counts more tokens than bytes).
    token_ratio: Mutex<f64>,
    /// Why today's spend could not be read (calls are refused meanwhile); `Judge::warning`.
    spend_error: Mutex<Option<String>>,
    stats: Mutex<JevStats>,
}

/// `criteria` keeps the (shuffled) order of its options when serialised.
struct Criteria(Vec<(String, String)>);

impl Serialize for Criteria {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

#[derive(Serialize)]
struct QuestionBody<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: &'a str,
    criteria: Criteria,
}

#[derive(Serialize)]
struct RequestBody<'a> {
    model: &'a str,
    state: &'a str,
    questions: BTreeMap<&'a str, QuestionBody<'a>>,
}

#[derive(Deserialize)]
struct ResponseBody {
    answers: BTreeMap<String, AnswerBody>,
    #[serde(default)]
    usage: UsageBody,
}

#[derive(Deserialize)]
struct AnswerBody {
    #[serde(default)]
    choice: Option<String>,
    #[serde(default)]
    probabilities: BTreeMap<String, f64>,
}

#[derive(Deserialize, Default)]
struct UsageBody {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

fn seed(parts: &[&str]) -> u64 {
    let h = sha256_hex(parts.join("\u{1f}").as_bytes());
    u64::from_str_radix(&h[..16], 16).unwrap_or(0x9e37_79b9_7f4a_7c15) | 1
}

/// Fisher–Yates with xorshift64, seeded per question.
fn shuffled(options: &[Choice], mut s: u64) -> Vec<Choice> {
    let mut v = options.to_vec();
    for i in (1..v.len()).rev() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        v.swap(i, (s % (i as u64 + 1)) as usize);
    }
    v
}

fn parse(body: &[u8], questions: &[Question]) -> JudgeResult<(BTreeMap<String, Distribution>, UsageBody)> {
    let r: ResponseBody = serde_json::from_slice(body)
        .map_err(|e| JudgeError::Failed(format!("Jev sent an answer that could not be read ({e})")))?;
    let mut out = BTreeMap::new();
    for q in questions {
        let a = r
            .answers
            .get(&q.id)
            .ok_or_else(|| JudgeError::Failed(format!("Jev answered without question {}", q.id)))?;
        let known = |k: &str| q.options.iter().any(|o| o.id == k);
        let mut d = Distribution::from_pairs(a.probabilities.iter().filter(|(k, _)| known(k)).map(|(k, v)| (k.clone(), *v)));
        if d.0.is_empty()
            && let Some(c) = a.choice.as_deref().filter(|c| known(c))
        {
            d = Distribution::certain(c);
        }
        if d.0.is_empty() {
            return Err(JudgeError::Failed(format!("Jev's answer to {} names none of the options", q.id)));
        }
        out.insert(q.id.clone(), d);
    }
    Ok((out, r.usage))
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500 * 2u64.pow(attempt.min(5)))
}

impl JevJudge {
    pub fn new(cfg: JevConfig, transport: Arc<dyn JevTransport>, store: Arc<dyn JevStore>, redactor: Arc<Redactor>) -> Self {
        Self {
            cfg,
            transport,
            store,
            redactor,
            clock: Arc::new(Utc::now),
            sleep: Arc::new(std::thread::sleep),
            next_slot: Mutex::new(None),
            budget: Mutex::new(None),
            token_ratio: Mutex::new(1.0),
            spend_error: Mutex::new(None),
            stats: Mutex::new(JevStats::default()),
        }
    }

    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_sleeper(mut self, sleep: Sleeper) -> Self {
        self.sleep = sleep;
        self
    }

    pub fn stats(&self) -> JevStats {
        self.stats.lock().clone()
    }

    fn denied(&self, project: &str) -> bool {
        self.cfg.cloud_denied.iter().any(|p| p == project)
    }

    /// The exact bytes `ask` would send for this (unredacted) state.
    pub fn request_body(&self, state: &str, questions: &[Question]) -> Vec<u8> {
        let state = self.redactor.apply(state).text;
        let mut parts: Vec<&str> = vec![&self.cfg.model, &state];
        for q in questions {
            parts.push(&q.id);
            parts.push(&q.instructions);
            for o in &q.options {
                parts.push(&o.id);
                parts.push(&o.description);
            }
        }
        let base = seed(&parts).to_string();
        let shuffled_options: Vec<Vec<Choice>> = questions
            .iter()
            .map(|q| shuffled(&q.options, seed(&[&base, &q.id])))
            .collect();
        let body = RequestBody {
            model: &self.cfg.model,
            state: &state,
            questions: questions
                .iter()
                .zip(&shuffled_options)
                .map(|(q, opts)| {
                    (
                        q.id.as_str(),
                        QuestionBody {
                            kind: "choice",
                            instructions: &q.instructions,
                            criteria: Criteria(opts.iter().map(|o| (o.id.clone(), o.description.clone())).collect()),
                        },
                    )
                })
                .collect(),
        };
        serde_json::to_vec(&body).expect("request serialises")
    }

    fn wait_for_slot(&self) {
        let gap = Duration::from_secs_f64(1.0 / self.cfg.rate_per_sec.clamp(0.1, 10.0));
        let wait = {
            let mut next = self.next_slot.lock();
            let now = Instant::now();
            let slot = next.map_or(now, |n| n.max(now));
            *next = Some(slot + gap);
            slot.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            (self.sleep)(wait);
        }
    }

    /// Reserve `estimate` USD of today's budget, or refuse.
    fn reserve(&self, day: &str, estimate: f64) -> JudgeResult<()> {
        let mut b = self.budget.lock();
        if b.as_ref().is_none_or(|b| b.day != day) {
            // Fail closed: a spend that cannot be read counts as an exhausted budget (it is read
            // again on the next call, and the warning shows in health until then).
            match self.store.spent(day) {
                Ok(spent) => {
                    *self.spend_error.lock() = None;
                    *b = Some(Budget { day: day.into(), spent });
                }
                Err(e) => {
                    let msg = format!("cannot read today's Jev spend ({e}); Jev calls are paused until it can be read");
                    *self.spend_error.lock() = Some(msg.clone());
                    self.stats.lock().budget_stops += 1;
                    return Err(JudgeError::Budget(msg));
                }
            }
        }
        let b = b.as_mut().expect("set above");
        if b.spent + estimate > self.cfg.daily_budget_usd {
            self.stats.lock().budget_stops += 1;
            return Err(JudgeError::Budget(format!(
                "${:.4} of ${:.2} spent on {day}; raise judge.daily_budget_usd or wait for the next UTC day",
                b.spent, self.cfg.daily_budget_usd
            )));
        }
        b.spent += estimate;
        Ok(())
    }

    fn settle(&self, day: &str, estimate: f64, actual: f64) {
        if let Some(b) = self.budget.lock().as_mut()
            && b.day == day
        {
            b.spent += actual - estimate;
        }
    }

    pub fn ask(&self, state: &str, questions: &[Question]) -> JudgeResult<BTreeMap<String, Distribution>> {
        let body = self.request_body(state, questions);
        let hash = sha256_hex(&body);
        if let Some(cached) = self.store.cached(&hash)
            && let Ok((d, _)) = parse(cached.as_bytes(), questions)
        {
            self.stats.lock().cache_hits += 1;
            return Ok(d);
        }
        let day = (self.clock)().format("%Y-%m-%d").to_string();
        // An upper bound on the cost: one input token per byte of the request (a token never
        // covers less than one byte), so the day's spend never passes the budget. Should Jev
        // ever count more tokens than bytes (a prompt of its own), the ratio rises to match.
        let estimate = body.len() as f64 * *self.token_ratio.lock() * USD_PER_MTOK_INPUT / 1e6;
        self.reserve(&day, estimate)?;
        let mut last = String::new();
        for attempt in 0..=self.cfg.max_retries {
            self.wait_for_slot();
            let started = Instant::now();
            let delay = match self.transport.post(&self.cfg.endpoint, &self.cfg.api_key, &body, self.cfg.timeout) {
                Ok(r) if r.status == 200 => {
                    let parsed = parse(&r.body, questions);
                    let (d, usage) = match parsed {
                        Ok(x) => x,
                        Err(e) => {
                            self.settle(&day, estimate, 0.0);
                            self.fail(&e);
                            return Err(e);
                        }
                    };
                    let usd = usage.input_tokens as f64 * USD_PER_MTOK_INPUT / 1e6;
                    self.settle(&day, estimate, usd);
                    let ratio = usage.input_tokens as f64 / body.len().max(1) as f64;
                    {
                        let mut r = self.token_ratio.lock();
                        if ratio > *r {
                            tracing::warn!(ratio, "Jev counted more input tokens than request bytes; budget estimates scale up");
                            *r = ratio;
                        }
                    }
                    let latency_ms = started.elapsed().as_millis() as u64;
                    self.store.record(&JevCall {
                        day: day.clone(),
                        hash: hash.clone(),
                        model: self.cfg.model.clone(),
                        response_json: String::from_utf8_lossy(&r.body).into_owned(),
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                        usd,
                        latency_ms,
                    });
                    let mut s = self.stats.lock();
                    s.calls += 1;
                    if s.latencies_ms.len() == 1_000 {
                        s.latencies_ms.remove(0);
                    }
                    s.latencies_ms.push(latency_ms);
                    return Ok(d);
                }
                Ok(r) if r.status == 401 || r.status == 403 => {
                    self.settle(&day, estimate, 0.0);
                    let e = JudgeError::Unavailable(format!(
                        "TypeSafe refused the API key ({}); set a valid one with `alice config set-secret jev_api_key`",
                        r.status
                    ));
                    self.fail(&e);
                    return Err(e);
                }
                Ok(r) if r.status == 429 || r.status >= 500 => {
                    last = format!("Jev answered {}", r.status);
                    r.retry_after.unwrap_or_else(|| backoff(attempt))
                }
                Ok(r) => {
                    self.settle(&day, estimate, 0.0);
                    let snippet: String = String::from_utf8_lossy(&r.body).chars().take(200).collect();
                    let e = JudgeError::Failed(format!("Jev answered {}: {snippet}", r.status));
                    self.fail(&e);
                    return Err(e);
                }
                Err(TransportError::Timeout) => {
                    last = "Jev did not answer in time".into();
                    backoff(attempt)
                }
                Err(TransportError::Connect(e)) => {
                    // Spec §5.2: retries only on 429, 5xx and timeouts. Nothing reached Jev: the
                    // heuristic decides now and the judge stage asks again later.
                    self.settle(&day, estimate, 0.0);
                    let e = JudgeError::Unavailable(format!("cannot reach Jev ({e}); will retry later"));
                    self.fail(&e);
                    return Err(e);
                }
            };
            if attempt < self.cfg.max_retries {
                self.stats.lock().retries += 1;
                (self.sleep)(delay.min(MAX_RETRY_WAIT));
            }
        }
        self.settle(&day, estimate, 0.0);
        let e = JudgeError::Unavailable(format!("{last}; will retry later"));
        self.fail(&e);
        Err(e)
    }

    fn fail(&self, e: &JudgeError) {
        let mut s = self.stats.lock();
        s.failures += 1;
        s.last_error = Some(e.to_string());
    }

    fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }
}

impl Judge for JevJudge {
    fn provider(&self) -> &str {
        "jev"
    }

    fn warning(&self) -> Option<String> {
        self.spend_error.lock().clone()
    }

    fn classify(&self, state: &str, options: &[Choice]) -> JudgeResult<Distribution> {
        let q = Question { id: "q".into(), instructions: "Choose the option that fits best.".into(), options: options.to_vec() };
        self.ask(state, std::slice::from_ref(&q))?.remove("q").ok_or_else(|| JudgeError::Failed("no answer".into()))
    }

    fn score(&self, state: &str, rubric: &Rubric) -> JudgeResult<Score> {
        let d = self.classify(&format!("{}\n\n{state}", rubric.question), &rubric.levels)?;
        let n = rubric.levels.len().max(2) as f64 - 1.0;
        let value = rubric.levels.iter().enumerate().map(|(i, l)| d.p(&l.id) * i as f64 / n).sum();
        Ok(Score { value, confidence: d.best().map_or(0.0, |b| b.1) })
    }

    fn compare(&self, new: &JudgeMemory, existing: &[JudgeMemory]) -> JudgeResult<Vec<Relation>> {
        if self.denied(&new.project) {
            return Err(JudgeError::Denied);
        }
        let now = self.now();
        let allowed: Vec<(usize, &JudgeMemory)> = existing.iter().enumerate().filter(|(_, e)| !self.denied(&e.project)).collect();
        let mut state = memory_block("New memory", new, now);
        let mut questions = Vec::new();
        for (n, (_, e)) in allowed.iter().enumerate() {
            let k = n + 1;
            state.push('\n');
            state.push_str(&memory_block(&format!("Existing memory {k}"), e, now));
            questions.push(Question { id: format!("fwd_{k}"), instructions: format!("How does the new memory relate to existing memory {k}?"), options: relation_choices() });
            questions.push(Question { id: format!("rev_{k}"), instructions: format!("How does existing memory {k} relate to the new memory?"), options: reverse_relation_choices() });
        }
        let answers = if questions.is_empty() { BTreeMap::new() } else { self.ask(&state, &questions)? };
        Ok(existing
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let Some(n) = allowed.iter().position(|(j, _)| *j == i) else {
                    return Relation { existing_id: e.id.clone(), probs: Distribution::certain("unrelated"), agreement: None };
                };
                let k = n + 1;
                let fwd = answers.get(&format!("fwd_{k}")).cloned().unwrap_or_default();
                let rev = answers.get(&format!("rev_{k}")).cloned().unwrap_or_default();
                let rev = Distribution::from_pairs(rev.0.into_iter().map(|(id, p)| (if id == "outdated_by" { "updates".to_string() } else { id }, p)));
                let agreement = match (fwd.best(), rev.best()) {
                    (Some(a), Some(b)) => Some(a.0 == b.0),
                    _ => None,
                };
                Relation { existing_id: e.id.clone(), probs: Distribution::average(&fwd, &rev), agreement }
            })
            .collect())
    }

    fn classify_kind(&self, m: &JudgeMemory) -> JudgeResult<Distribution> {
        if self.denied(&m.project) {
            return Err(JudgeError::Denied);
        }
        self.classify(&kind_state(m, self.now()), &kind_choices())
    }

    fn route_project(&self, m: &JudgeMemory, projects: &[ProjectOption]) -> JudgeResult<Distribution> {
        if self.denied(&m.project) || self.denied("global") {
            return Err(JudgeError::Denied);
        }
        let options: Vec<Choice> = projects
            .iter()
            .filter(|p| !self.denied(&p.slug))
            .map(|p| Choice::new(&p.slug, &format!("{}: {}", p.name, p.hint)))
            .chain([Choice::new("global", "not about any one project")])
            .collect();
        self.classify(&format!("Which project does this memory belong to?\n\n{}", memory_block("Memory", m, self.now())), &options)
    }

    fn grade_relevance(&self, query: &str, results: &[JudgeMemory]) -> JudgeResult<Vec<Distribution>> {
        let now = self.now();
        let mut state = format!("Query: {query}\n");
        let mut questions = Vec::new();
        let mut asked = Vec::new();
        for (i, r) in results.iter().enumerate() {
            if self.denied(&r.project) {
                continue;
            }
            let k = i + 1;
            state.push('\n');
            state.push_str(&memory_block(&format!("Result {k}"), r, now));
            questions.push(Question { id: format!("r_{k}"), instructions: format!("How well does result {k} answer the query?"), options: relevance_choices() });
            asked.push(i);
        }
        let answers = if questions.is_empty() { BTreeMap::new() } else { self.ask(&state, &questions)? };
        Ok((0..results.len())
            .map(|i| if asked.contains(&i) { answers.get(&format!("r_{}", i + 1)).cloned().unwrap_or_default() } else { Distribution::default() })
            .collect())
    }
}

/// Fixed inputs shared by the unit tests and the fixture recorder (`jev_live.rs`), so the
/// recorded requests match what the code builds.
pub mod scenarios {
    use crate::judge::JudgeMemory;
    use crate::model::Kind;
    use chrono::{DateTime, TimeZone, Utc};

    pub fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap()
    }

    fn m(id: &str, kind: Kind, title: &str, body: &str, days_ago: i64) -> JudgeMemory {
        JudgeMemory {
            id: id.into(), project: "alice".into(), kind, title: title.into(), body: body.into(), importance: 3,
            created_at: (fixed_now() - chrono::Duration::days(days_ago)).to_rfc3339(), similarity: None,
        }
    }

    /// A new decision against an older version of it and an unrelated fact.
    pub fn compare_case() -> (JudgeMemory, Vec<JudgeMemory>) {
        (
            m("NEW", Kind::Decision, "Embeddings run on ONNX Runtime directly", "We dropped fastembed: the daemon runs the int8 e5 model on ONNX Runtime with a compact tokenizer, which saves about 250 MB.", 0),
            vec![
                m("OLD", Kind::Decision, "Embeddings run through fastembed", "The daemon embeds with fastembed 7.1 and the Hugging Face tokenizer.", 14),
                m("FACT", Kind::Fact, "Thai search uses trigram FTS5", "Thai has no spaces between words, so lexical search uses the FTS5 trigram tokenizer.", 30),
            ],
        )
    }

    /// What the recorded compare fixture chose for each existing memory.
    pub const COMPARE_EXPECTED: [&str; 2] = ["updates", "unrelated"];

    pub fn kind_case() -> JudgeMemory {
        m("K", Kind::Note, "Writer lease held during mining", "Symptom: diary writes timed out after 120 s. Cause: the hub held the writer lease while mining. Fix: mining now runs off the writer.", 1)
    }

    pub const KIND_EXPECTED: &str = "bugfix";

    pub fn relevance_case() -> (String, Vec<JudgeMemory>) {
        (
            "why did diary writes time out?".into(),
            vec![
                kind_case(),
                m("R2", Kind::Fact, "Spool directory", "Writes go to ~/.alice/spool when the daemon is down.", 3),
                m("R3", Kind::Preference, "Answer in Thai", "The user wants answers in Thai.", 9),
            ],
        )
    }
}
```

(Fixture files are recorded in Step 8; until then create each as `{"request": null, "response": null}` so the crate compiles — the fixture tests fail until Step 8, the scripted ones pass now.)

Run: `nice -n 15 cargo test -j 6 -p alice-core judge::jev` — Expected: the scripted tests pass; the two tests that post through `Fixtures` fail with "no fixture for this request".

- [ ] **Step 5: The daemon side**

`crates/alice-daemon/src/judge_http.rs`:

```rust
//! Jev for the daemon: HTTP through `BlockingHttp`, cache and costs through the reader pool and
//! the writer, and choosing the judge from settings.

use crate::secrets::{SecretName, SecretStore};
use crate::writer::{WriteOp, WriterHandle};
use alice_core::judge::Judge;
use alice_core::judge::heuristic::HeuristicJudge;
use alice_core::judge::jev::{HttpReply, JevCall, JevConfig, JevJudge, JevStore, JevTransport, TransportError};
use alice_core::redact::Redactor;
use alice_core::settings::{JudgeProvider, PrivacySettings, Settings};
use alice_core::store::ReaderPool;
use alice_embed::net::{BlockingHttp, HttpError};
use std::sync::Arc;
use std::time::Duration;

pub struct ReqwestTransport {
    http: BlockingHttp,
}

impl ReqwestTransport {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self { http: BlockingHttp::new(concat!("alice/", env!("CARGO_PKG_VERSION")))? })
    }
}

impl JevTransport for ReqwestTransport {
    fn post(&self, endpoint: &str, key: &str, body: &[u8], timeout: Duration) -> Result<HttpReply, TransportError> {
        match self.http.post_json(endpoint, Some(key), body.to_vec(), timeout) {
            Ok(r) => {
                let retry_after = r
                    .headers
                    .iter()
                    .find(|(k, _)| k == "retry-after")
                    .and_then(|(_, v)| v.trim().parse::<u64>().ok())
                    .map(Duration::from_secs);
                Ok(HttpReply { status: r.status, body: r.body, retry_after })
            }
            Err(HttpError::Timeout) => Err(TransportError::Timeout),
            Err(HttpError::Connect(e) | HttpError::Other(e)) => Err(TransportError::Connect(e)),
        }
    }
}

pub struct DaemonJevStore {
    pub readers: Arc<ReaderPool>,
    pub writer: WriterHandle,
}

impl JevStore for DaemonJevStore {
    fn cached(&self, hash: &str) -> Option<String> {
        self.readers.with(|c| alice_core::store::judge::cache_get(c, hash)).ok().flatten()
    }
    fn spent(&self, day: &str) -> Result<f64, String> {
        // No fallback to 0: an unreadable spend must stop calls, not allow a full new budget.
        self.readers.with(|c| alice_core::store::judge::spent(c, day, "jev")).map_err(|e| e.to_string())
    }
    fn record(&self, call: &JevCall) {
        self.writer.send_nowait(WriteOp::JudgeCall(call.clone()));
    }
}

/// The judge `settings` ask for, and a warning when it had to fall back to the heuristic.
pub fn build_judge(
    settings: &Settings,
    secrets: &dyn SecretStore,
    readers: Arc<ReaderPool>,
    writer: WriterHandle,
    transport: Option<Arc<dyn JevTransport>>,
) -> (Arc<dyn Judge>, Option<String>) {
    if settings.judge.provider == JudgeProvider::Heuristic {
        return (Arc::new(HeuristicJudge::default()), None);
    }
    let key = match crate::secrets::resolve(secrets, SecretName::JevApiKey) {
        Ok(Some((k, _))) => k,
        _ => {
            return (
                Arc::new(HeuristicJudge::default()),
                Some("judge.provider is jev but no TypeSafe key is set; using the heuristic judge. Set one with `alice config set-secret jev_api_key`".into()),
            );
        }
    };
    let transport = match transport {
        Some(t) => t,
        None => match ReqwestTransport::new() {
            Ok(t) => Arc::new(t),
            Err(e) => return (Arc::new(HeuristicJudge::default()), Some(format!("cannot start the HTTP client for Jev ({e}); using the heuristic judge"))),
        },
    };
    // Judge requests are always redacted, whatever privacy.redact says.
    let privacy = PrivacySettings { redact: true, ..settings.privacy.clone() };
    let redactor = match Redactor::new(&privacy) {
        Ok(r) => Arc::new(r),
        Err(e) => return (Arc::new(HeuristicJudge::default()), Some(format!("privacy settings: {e}; using the heuristic judge"))),
    };
    let cfg = JevConfig::from_settings(&settings.judge, &settings.privacy, key);
    (Arc::new(JevJudge::new(cfg, transport, Arc::new(DaemonJevStore { readers, writer }), redactor)), None)
}
```

`writer.rs`: `WriteOp::JudgeCall(JevCall)` → `store::judge::cache_put(conn, &c.hash, "jev", &c.model, &c.response_json)` and `store::judge::add_cost(conn, &CostDelta { day: c.day, provider: "jev".into(), calls: 1, input_tokens: c.input_tokens, output_tokens: c.output_tokens, usd: c.usd })`.

In `judge/mod.rs` add to the `Judge` trait (Task 8) the default method `/// A condition worth showing in health (e.g. Jev paused because its spend cannot be read).\n    fn warning(&self) -> Option<String> { None }`.

`service.rs`: field `judge: RwLock<(Arc<dyn Judge>, Option<String>)>` built in `start` with `build_judge(&settings.get(), secrets.as_ref(), readers.clone(), writer.clone(), opts.jev_transport.clone())`; `fn rebuild_judge(&self)`; `judge_warning()` returns the slot's build warning, else `self.judge().warning()`; Task 6's config-events `on_change` closure (`follow_settings`, which also re-reads after a lag) also calls a rebuild when a changed key starts with `judge.` or `privacy.` (give that closure the pieces `build_judge` needs and the `Arc<RwLock<…>>` slot, not the `Service`); `set_secret`/`delete_secret` for `JevApiKey` rebuild too. `test_provider("judge")`:

```rust
            "judge" => {
                let j = self.judge();
                let provider = j.provider().to_string();
                use alice_core::judge::jev::scenarios;
                let r = tokio::task::spawn_blocking(move || j.classify(scenarios::CLASSIFY_STATE, &scenarios::classify_choices()))
                    .await
                    .map_err(join_err)?;
                Ok(match (provider.as_str(), r) {
                    ("heuristic", _) => ok(self.judge_warning().unwrap_or_else(|| "the heuristic judge needs no connection".into())),
                    (_, Ok(d)) => ok(format!("Jev answered ({:?})", d.best())),
                    (_, Err(e)) => ProviderTest { ok: false, message: e.to_string(), latency_ms: started.elapsed().as_millis() as u64 },
                })
            }
```

Service test with a scripted transport injected through `ServiceOptions::jev_transport`: set `judge.provider = jev` via `patch_settings` and a key via `set_secret(JevApiKey, …)`, call `test_provider("judge")`, assert `ok`, then wait for the `cost_ledger` row (it is written fire-and-forget through `WriteOp::JudgeCall`): poll `SELECT count(*) FROM cost_ledger WHERE day = <today>` through `s.read` every 20 ms until it is 1, failing after 5 s. Note the judge rebuild after `patch_settings` also runs on the config-events thread: poll `s.judge().provider() == "jev"` the same way before calling `test_provider`.

- [ ] **Step 6: Run the workspace tests**

Run: `nice -n 15 cargo test -j 6 --workspace` — Expected: pass except the two tests that post through `Fixtures` (still empty fixtures).

- [ ] **Step 7: The live smoke test and fixture recorder**

`crates/alice-daemon/tests/jev_live.rs`:

```rust
//! Live TypeSafe Jev checks (network, costs a fraction of a cent). The key comes only from the
//! environment; load it from the repo's git-ignored .env without printing it:
//!   set -a; . ./.env; set +a
//!   cargo test -p alice-daemon --test jev_live -- --ignored --nocapture
use alice_core::judge::Judge;
use alice_core::judge::jev::{JevConfig, JevJudge, MemoryJevStore, scenarios};
use alice_core::redact::Redactor;
use alice_daemon::judge_http::ReqwestTransport;
use std::sync::Arc;
use std::time::Duration;

fn key() -> String {
    std::env::var("TYPESAFE_API_KEY").expect("set TYPESAFE_API_KEY (set -a; . ./.env; set +a)")
}

fn judge(store: Arc<MemoryJevStore>) -> JevJudge {
    let cfg = JevConfig {
        endpoint: "https://api.typesafe.ai/v1/systemone".into(),
        model: "jev-1.13.0".into(),
        api_key: key(),
        rate_per_sec: 5.0,
        timeout: Duration::from_secs(30),
        daily_budget_usd: 0.05,
        max_retries: 3,
        cloud_denied: vec![],
    };
    JevJudge::new(cfg, Arc::new(ReqwestTransport::new().unwrap()), store, Arc::new(Redactor::builtin()))
        .with_clock(Arc::new(scenarios::fixed_now))
}

#[test]
#[ignore = "network: needs TYPESAFE_API_KEY"]
fn live_compare_round_trip() {
    let store = Arc::new(MemoryJevStore::default());
    let j = judge(store.clone());
    let (new, existing) = scenarios::compare_case();
    let rel = j.compare(&new, &existing).unwrap();
    for r in &rel {
        let total: f64 = r.probs.0.values().sum();
        assert!((total - 1.0).abs() < 1e-6);
        println!("{}: {:?} (forward/reversed agree: {:?})", r.existing_id, r.probs.best(), r.agreement);
    }
    let calls = store.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].input_tokens > 0 && calls[0].usd > 0.0);
    println!("input tokens {}, cost ${:.6}, latency {} ms", calls[0].input_tokens, calls[0].usd, calls[0].latency_ms);
}

/// Writes crates/alice-core/tests/fixtures/jev/*.json: the request the code built (captured
/// with `last_request`) and the live response body. Headers, and so the key, are never written.
#[test]
#[ignore = "network: re-records the Jev fixtures"]
fn record_fixtures() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../alice-core/tests/fixtures/jev");
    let store = Arc::new(MemoryJevStore::default());
    let j = judge(store.clone());
    let mut requests = Vec::new();
    let (new, existing) = scenarios::compare_case();
    j.compare(&new, &existing).unwrap();
    requests.push(("compare", j.last_request().unwrap()));
    j.classify_kind(&scenarios::kind_case()).unwrap();
    requests.push(("kind", j.last_request().unwrap()));
    let (q, results) = scenarios::relevance_case();
    j.grade_relevance(&q, &results).unwrap();
    requests.push(("relevance", j.last_request().unwrap()));
    j.classify(scenarios::CLASSIFY_STATE, &scenarios::classify_choices()).unwrap();
    requests.push(("classify", j.last_request().unwrap()));
    let calls = store.calls();
    assert_eq!(calls.len(), 4, "each request went to the API once");
    for ((name, request), call) in requests.into_iter().zip(calls) {
        let request: serde_json::Value = serde_json::from_slice(&request).unwrap();
        let response: serde_json::Value = serde_json::from_str(&call.response_json).unwrap();
        let text = serde_json::to_string_pretty(&serde_json::json!({"recorded": true, "request": request, "response": response})).unwrap();
        assert!(!text.contains(&key()), "the key never goes into a fixture");
        std::fs::write(dir.join(format!("{name}.json")), text + "\n").unwrap();
    }
}
```

`JevJudge` keeps the last body it built for this: add a field `last_request: Mutex<Option<Vec<u8>>>` (initialised `None`), set it in `ask` right after `request_body`, and

```rust
    /// The body of the most recent request built (sent or answered from the cache).
    pub fn last_request(&self) -> Option<Vec<u8>> {
        self.last_request.lock().clone()
    }
```

and add to `scenarios`:

```rust
    pub const CLASSIFY_STATE: &str = "This is a connection test from Alice. Answer yes.";

    pub fn classify_choices() -> Vec<crate::judge::Choice> {
        vec![
            crate::judge::Choice::new("yes", "this is a test"),
            crate::judge::Choice::new("no", "this is not a test"),
        ]
    }
```

(Use `scenarios::CLASSIFY_STATE` / `classify_choices()` in `Service::test_provider("judge")` too, and add `assert_eq!(j.classify(scenarios::CLASSIFY_STATE, &scenarios::classify_choices()).unwrap().best().unwrap().0, "yes");` to `kind_and_relevance_fixtures_parse`.)

- [ ] **Step 8: Record the fixtures (one live run), then replay them offline**

```bash
cd /Users/icue/Code/AI/project-alice
export S=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2 ALICE_HOME=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2/home; mkdir -p "$ALICE_HOME"; [ -e "$ALICE_HOME/models" ] || ln -s "$HOME/.alice/models" "$ALICE_HOME/models"   # scratch prelude (Global Constraints)
set -a; . ./.env; set +a
nice -n 15 cargo test -j 6 -p alice-daemon --test jev_live -- --ignored --nocapture
unset TYPESAFE_API_KEY
grep -c "Authorization\|Bearer" crates/alice-core/tests/fixtures/jev/*.json   # expect 0 for every file
nice -n 15 cargo test -j 6 -p alice-core judge::jev
```

Expected: the live compare prints probabilities, tokens (~1–2k) and a cost well under $0.001; four fixture files are written; the offline tests now pass. If `COMPARE_EXPECTED` or `KIND_EXPECTED` disagree with what Jev answered, the scenario is ambiguous — make the scenario text clearer and re-record; do not edit the expected values to match a surprising answer. If the API cannot be reached (no key, no network), write the four fixtures by hand in the same shape (`"recorded": false`, the request from `last_request`, a response with plausible probabilities) and say so in the task report; the live test then stays to be run by the owner.

- [ ] **Step 9: Commit**

```bash
nice -n 15 cargo fmt --all && nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
nice -n 15 cargo test -j 6 --workspace
git add crates
git commit -m "feat: Jev judge with request cache, daily budget stop, rate limit, retries and recorded fixtures

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 10: Jev evaluation — labelled set, metrics, `docs/benchmarks/m2-jev.md`

The owner asked for Alice to be measured with Jev (ledger, 2026-10-13). This task builds a committed synthetic set of ≥ 200 labelled relation pairs in Thai, English, mixed Thai–English and code, and an ignored evaluation that reports, for Jev and the heuristic: relation accuracy, macro-F1, calibration (Brier score, expected calibration error), auto-apply precision and coverage at 0.85, Jev's forward/reversed agreement, kind-classification accuracy, Jev-graded nDCG@10 of the M1 search (hybrid vs lexical-only vs vector-only, real model), latency p50/p95 and cost per 1,000 decisions — under a hard $2 budget for the whole run.

**Files:**
- Create: `crates/alice-core/src/judge/metrics.rs` (+ `pub mod metrics;`)
- Modify: `crates/alice-core/src/search/mod.rs` (`SearchParams::lists: Lists`)
- Create: `crates/alice-daemon/tests/fixtures/judge_eval_pairs.jsonl` (≥ 200 lines), `crates/alice-daemon/tests/fixtures/judge_eval_queries.jsonl` (≥ 40 lines)
- Create: `crates/alice-daemon/tests/judge_eval_fixture.rs` (runs in CI: the set is balanced and clean), `crates/alice-daemon/tests/jev_eval.rs` (ignored: the evaluation)
- Create: `docs/benchmarks/m2-jev.md`

**Interfaces:**
- Consumes: Task 8 (`HeuristicJudge`, `kind_rules`, `Distribution`, `RelationKind`, `JudgeMemory`, `RELEVANCE_*`), Task 9 (`JevJudge`, `JevConfig`, `MemoryJevStore`, `JevStats`, `ReqwestTransport`), Task 2 (`OnnxEmbedder`), M1 `search::run`, `VectorIndex::for_model`, `memories::{prepare, insert}`.
- Produces:
  - `alice_core::judge::metrics::{accuracy(gold: &[String], pred: &[String]) -> f64, macro_f1(gold: &[String], pred: &[String], labels: &[&str]) -> f64, brier(gold: &[String], probs: &[Distribution], labels: &[&str]) -> f64, ece(gold: &[String], probs: &[Distribution], bins: usize) -> f64, dcg(grades: &[f64], k: usize) -> f64, ndcg_at(grades_in_rank_order: &[f64], pool: &[f64], k: usize) -> f64, percentile(v: &mut [f64], p: f64) -> f64}`
  - `alice_core::search::Lists::{Hybrid (default), LexicalOnly, VectorOnly}`; `SearchParams::lists`.
  - The eval prints the markdown tables that go into `docs/benchmarks/m2-jev.md` and saves them to `target/m2-jev-eval.md` (or `$ALICE_EVAL_OUT`).

- [ ] **Step 1: Read the files this task modifies, as they are now.**

- [ ] **Step 2: Metrics, test first**

`crates/alice-core/src/judge/metrics.rs`:

```rust
//! Evaluation metrics for judges and search.

use super::Distribution;

pub fn accuracy(gold: &[String], pred: &[String]) -> f64 {
    if gold.is_empty() {
        return 0.0;
    }
    gold.iter().zip(pred).filter(|(g, p)| g == p).count() as f64 / gold.len() as f64
}

/// Mean of per-label F1 (a label never predicted nor present is skipped).
pub fn macro_f1(gold: &[String], pred: &[String], labels: &[&str]) -> f64 {
    let mut f1s = Vec::new();
    for l in labels {
        let tp = gold.iter().zip(pred).filter(|(g, p)| g.as_str() == *l && p.as_str() == *l).count() as f64;
        let fp = gold.iter().zip(pred).filter(|(g, p)| g.as_str() != *l && p.as_str() == *l).count() as f64;
        let fn_ = gold.iter().zip(pred).filter(|(g, p)| g.as_str() == *l && p.as_str() != *l).count() as f64;
        if tp + fp + fn_ == 0.0 {
            continue;
        }
        f1s.push(if tp == 0.0 { 0.0 } else { 2.0 * tp / (2.0 * tp + fp + fn_) });
    }
    if f1s.is_empty() { 0.0 } else { f1s.iter().sum::<f64>() / f1s.len() as f64 }
}

/// Multi-class Brier score: mean over items of Σ_label (p − 1[label = gold])². 0 is perfect, 2 the worst.
pub fn brier(gold: &[String], probs: &[Distribution], labels: &[&str]) -> f64 {
    if gold.is_empty() {
        return 0.0;
    }
    gold.iter()
        .zip(probs)
        .map(|(g, d)| labels.iter().map(|l| { let y = if g == l { 1.0 } else { 0.0 }; (d.p(l) - y).powi(2) }).sum::<f64>())
        .sum::<f64>()
        / gold.len() as f64
}

/// Expected calibration error over `bins` equal-width bins of the top probability.
pub fn ece(gold: &[String], probs: &[Distribution], bins: usize) -> f64 {
    let n = gold.len() as f64;
    if n == 0.0 {
        return 0.0;
    }
    let mut acc = vec![0.0; bins];
    let mut conf = vec![0.0; bins];
    let mut count = vec![0usize; bins];
    for (g, d) in gold.iter().zip(probs) {
        let Some((label, p)) = d.best() else { continue };
        let b = ((p * bins as f64) as usize).min(bins - 1);
        count[b] += 1;
        conf[b] += p;
        acc[b] += if &label == g { 1.0 } else { 0.0 };
    }
    (0..bins)
        .filter(|&b| count[b] > 0)
        .map(|b| (count[b] as f64 / n) * ((acc[b] - conf[b]) / count[b] as f64).abs())
        .sum()
}

pub fn dcg(grades: &[f64], k: usize) -> f64 {
    grades.iter().take(k).enumerate().map(|(i, g)| (2f64.powf(*g) - 1.0) / ((i + 2) as f64).log2()).sum()
}

/// nDCG@k of a ranking, with the ideal ranking taken from the pooled grades of every system.
pub fn ndcg_at(grades_in_rank_order: &[f64], pool: &[f64], k: usize) -> f64 {
    let mut ideal = pool.to_vec();
    ideal.sort_by(|a, b| b.total_cmp(a));
    let idcg = dcg(&ideal, k);
    if idcg == 0.0 { 0.0 } else { dcg(grades_in_rank_order, k) / idcg }
}

pub fn percentile(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    v[(((v.len() - 1) as f64) * p).round() as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn classification_metrics() {
        let gold = s(&["a", "a", "b", "c"]);
        let pred = s(&["a", "b", "b", "c"]);
        assert_eq!(accuracy(&gold, &pred), 0.75);
        // a: tp1 fp0 fn1 → 2/3; b: tp1 fp1 fn0 → 2/3; c: 1.
        assert!((macro_f1(&gold, &pred, &["a", "b", "c", "d"]) - (2.0 / 3.0 + 2.0 / 3.0 + 1.0) / 3.0).abs() < 1e-9);
        let certain_right: Vec<Distribution> = gold.iter().map(|g| Distribution::certain(g)).collect();
        assert_eq!(brier(&gold, &certain_right, &["a", "b", "c"]), 0.0);
        assert_eq!(ece(&gold, &certain_right, 10), 0.0);
        let wrong = vec![Distribution::certain("b"); 4];
        assert!((brier(&s(&["a"; 4]), &wrong, &["a", "b"]) - 2.0).abs() < 1e-9);
        assert!((ece(&s(&["a"; 4]), &wrong, 10) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ndcg_is_one_for_the_ideal_ranking_and_lower_otherwise() {
        let pool = [2.0, 1.0, 0.0, 0.0];
        assert!((ndcg_at(&[2.0, 1.0, 0.0], &pool, 10) - 1.0).abs() < 1e-9);
        assert!(ndcg_at(&[0.0, 1.0, 2.0], &pool, 10) < 0.8);
        assert_eq!(ndcg_at(&[0.0], &[0.0], 10), 0.0);
        assert_eq!(percentile(&mut [3.0, 1.0, 2.0], 0.5), 2.0);
    }
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-core judge::metrics` — Expected: 2 passed.

- [ ] **Step 3: Search lists switch, test first**

In `search/mod.rs`:

```rust
/// Which candidate lists recall uses (evaluation only; agents always get `Hybrid`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Lists {
    #[default]
    Hybrid,
    LexicalOnly,
    VectorOnly,
}
```

`SearchParams` gains `pub lists: Lists`; `Query` carries it; in `knowledge()` and `raw()` build the word/code/trigram/LIKE lists only when `lists != Lists::VectorOnly`, and the vector list only when `lists != Lists::LexicalOnly`. Test (in the module's tests, with the existing fixture helpers):

```rust
    #[test]
    fn lexical_only_and_vector_only_use_only_their_lists() {
        // Reuse this module's helper that builds a DB with a few memories and a fake index.
        let (c, idx, e) = small_corpus();
        for (lists, banned) in [(Lists::LexicalOnly, MatchVia::Vector), (Lists::VectorOnly, MatchVia::Words)] {
            let p = SearchParams { query: "writer lease timeout".into(), limit: 10, relevance_floor: 0.0, lists, ..Default::default() };
            let out = run(&c, Some(&idx), &e, &Weights::default(), &p, chrono::Utc::now()).unwrap();
            assert!(!out.hits.is_empty(), "{lists:?}");
            assert!(out.hits.iter().all(|h| !h.matched.contains(&banned)), "{lists:?}: {:?}", out.hits);
        }
    }
```

(If the module has no such helper, write `small_corpus()` in the test module: an `open_writer` temp DB, `upsert_project`, three memories via `memories::prepare/insert`, `FakeEmbedder::new(384)`, `VectorIndex::for_model("fake", 384)` with their vectors.)

Run: `nice -n 15 cargo test -j 6 -p alice-core search::` — Expected: pass.

- [ ] **Step 4: The labelled set**

`crates/alice-daemon/tests/fixtures/judge_eval_pairs.jsonl`, one JSON object per line:

```
{"id": <unique string>, "lang": "th"|"en"|"mixed"|"code", "label": "duplicate"|"updates"|"contradicts"|"unrelated",
 "new": {"kind": <kind>, "title": <string>, "body": <string>, "days_ago": <int>, "importance": <1-5, optional>},
 "old": {…same fields…}, "new_kind": <the true kind of the new memory>}
```

Composition (the CI check in Step 5 enforces it): at least 50 pairs per label (≥ 200 in all); per label at least 15 `th`, 15 `en`, 10 `mixed`, 10 `code`; every one of the 11 kinds is `new_kind` of at least 6 pairs; `updates` pairs have `new.days_ago < old.days_ago`; `contradicts` pairs include at least 15 with equal ages (so time order does not decide); `unrelated` pairs include at least 20 that share a topic or words with each other (the hard negatives); `duplicate` pairs include at least 20 paraphrases with little word overlap; every body ≤ 600 characters; no real secrets, names of real people, or customer data (all synthetic). Each label must be what a careful human reader would choose without discussion; drop a pair rather than keep an ambiguous one. Start from these twelve lines (they count towards the totals):

```jsonl
{"id":"dup-th-01","lang":"th","label":"duplicate","new":{"kind":"decision","title":"ตั้ง SQLite เป็น WAL mode","body":"ตั้งค่า WAL mode ให้ SQLite เพื่อให้ reader ไม่ต้องรอ writer","days_ago":0},"old":{"kind":"decision","title":"SQLite ใช้ WAL","body":"SQLite ใช้ WAL mode เพื่อให้การอ่านไม่ถูกบล็อกโดยการเขียน","days_ago":3},"new_kind":"decision"}
{"id":"dup-en-01","lang":"en","label":"duplicate","new":{"kind":"howto","title":"Deploying the API","body":"Deploy the API with `make deploy-prod`; it runs the migrations first.","days_ago":0},"old":{"kind":"howto","title":"API production deploy","body":"To put the API in production run make deploy-prod, which applies migrations before starting the service.","days_ago":20},"new_kind":"howto"}
{"id":"dup-code-01","lang":"code","label":"duplicate","new":{"kind":"fact","title":"HostLinkReconnect backoff","body":"`HostLinkReconnect` retries with exponential backoff capped at 30 s.","days_ago":0},"old":{"kind":"fact","title":"Reconnect policy","body":"HostLinkReconnect uses exponential backoff, at most 30 seconds between retries.","days_ago":9},"new_kind":"fact"}
{"id":"upd-en-01","lang":"en","label":"updates","new":{"kind":"fact","title":"Staging DB port","body":"The staging Postgres moved to port 5433 (it was 5432) on 2026-10-10.","days_ago":0},"old":{"kind":"fact","title":"Staging DB port","body":"Staging Postgres listens on port 5432.","days_ago":40},"new_kind":"fact"}
{"id":"upd-th-01","lang":"th","label":"updates","new":{"kind":"status","title":"ย้าย hosting เสร็จแล้ว","body":"ตอนนี้แอป deploy ด้วย Coolify แทน Railway แล้ว ย้ายเสร็จเมื่อวาน","days_ago":0},"old":{"kind":"status","title":"hosting ปัจจุบัน","body":"แอป deploy อยู่บน Railway","days_ago":60},"new_kind":"status"}
{"id":"upd-mixed-01","lang":"mixed","label":"updates","new":{"kind":"decision","title":"เปลี่ยนมาใช้ rustls","body":"เปลี่ยน reqwest มาใช้ `rustls` แทน `native-tls` แล้ว เพราะ build บน musl ง่ายกว่า","days_ago":1},"old":{"kind":"decision","title":"TLS ของ reqwest","body":"reqwest ใช้ native-tls (OpenSSL) ของระบบ","days_ago":90},"new_kind":"decision"}
{"id":"con-en-01","lang":"en","label":"contradicts","new":{"kind":"fact","title":"Cache TTL","body":"The response cache TTL is 10 minutes.","days_ago":2},"old":{"kind":"fact","title":"Cache TTL","body":"The response cache TTL is 1 hour.","days_ago":2},"new_kind":"fact"}
{"id":"con-th-01","lang":"th","label":"contradicts","new":{"kind":"preference","title":"ภาษาที่ใช้ตอบ","body":"ผู้ใช้ต้องการให้ตอบเป็นภาษาอังกฤษเสมอ","days_ago":5},"old":{"kind":"preference","title":"ภาษาที่ใช้ตอบ","body":"ผู้ใช้ต้องการให้ตอบเป็นภาษาไทยเสมอ","days_ago":5},"new_kind":"preference"}
{"id":"con-code-01","lang":"code","label":"contradicts","new":{"kind":"fact","title":"writer_lease during mining","body":"`writer_lease` is held for the whole mining run.","days_ago":3},"old":{"kind":"fact","title":"writer_lease during mining","body":"`writer_lease` is released between mining batches.","days_ago":3},"new_kind":"fact"}
{"id":"unr-en-01","lang":"en","label":"unrelated","new":{"kind":"howto","title":"Finding stray daemons","body":"Run `pgrep -fl 'alice daemon run'` after tests to find stray daemons.","days_ago":0},"old":{"kind":"fact","title":"Bench corpus","body":"The bench corpus mixes 2,000 made-up tail words into its sentences.","days_ago":12},"new_kind":"howto"}
{"id":"unr-th-01","lang":"th","label":"unrelated","new":{"kind":"warning","title":"ห้ามรัน migration เอง","body":"ระวัง: อย่ารัน migration บน production ด้วยมือ ให้ผ่าน pipeline เท่านั้น","days_ago":0},"old":{"kind":"fact","title":"โดเมนร้านค้า","body":"ร้านค้าตัวอย่างใช้โดเมน shop.example.com","days_ago":30},"new_kind":"warning"}
{"id":"unr-mixed-01","lang":"mixed","label":"unrelated","new":{"kind":"fact","title":"Thai search ใช้ trigram","body":"การค้นหาภาษาไทยใช้ trigram FTS5 เพราะไม่มีช่องว่างระหว่างคำ","days_ago":4},"old":{"kind":"fact","title":"Recall token budget","body":"Search results are packed to a 1,500-token budget by default.","days_ago":4},"new_kind":"fact"}
```

`judge_eval_queries.jsonl`: at least 40 lines `{"q": <query>, "lang": "th"|"en"|"mixed"|"code"}` about the memories in the pairs file (≥ 10 per language group), worded as an agent would ask (`{"q":"ทำไม reader ไม่ต้องรอ writer","lang":"th"}`, `{"q":"how do I deploy the API to production","lang":"en"}`, `{"q":"HostLinkReconnect backoff limit","lang":"code"}`, `{"q":"staging postgres port เปลี่ยนเป็นอะไร","lang":"mixed"}`), including at least 8 that no memory answers well (to test that grades go to zero).

- [ ] **Step 5: CI check of the set**

`crates/alice-daemon/tests/judge_eval_fixture.rs`:

```rust
//! The Jev evaluation set is balanced, well-formed and clean (runs in CI; no network).
use std::collections::{HashMap, HashSet};

#[test]
fn the_labelled_set_meets_its_composition_rules() {
    let pairs: Vec<serde_json::Value> = include_str!("fixtures/judge_eval_pairs.jsonl")
        .lines().filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect();
    assert!(pairs.len() >= 200, "{} pairs", pairs.len());
    let mut ids = HashSet::new();
    let mut per: HashMap<(String, String), usize> = HashMap::new();
    let mut kinds: HashMap<String, usize> = HashMap::new();
    let (mut same_age_contradicts, mut redactor_hits) = (0, 0);
    let r = alice_core::redact::Redactor::builtin();
    for p in &pairs {
        assert!(ids.insert(p["id"].as_str().unwrap().to_string()), "duplicate id {}", p["id"]);
        let label = p["label"].as_str().unwrap();
        assert!(alice_core::judge::RelationKind::parse(label).is_some(), "{label}");
        let lang = p["lang"].as_str().unwrap();
        assert!(["th", "en", "mixed", "code"].contains(&lang), "{lang}");
        *per.entry((label.into(), lang.into())).or_default() += 1;
        let k = p["new_kind"].as_str().unwrap();
        assert!(alice_core::model::Kind::parse(k).is_some(), "{k}");
        *kinds.entry(k.into()).or_default() += 1;
        for side in ["new", "old"] {
            let m = &p[side];
            assert!(alice_core::model::Kind::parse(m["kind"].as_str().unwrap()).is_some());
            let body = m["body"].as_str().unwrap();
            assert!(!body.is_empty() && body.chars().count() <= 600, "{}", p["id"]);
            redactor_hits += r.apply(body).found.len();
        }
        let (n, o) = (p["new"]["days_ago"].as_i64().unwrap(), p["old"]["days_ago"].as_i64().unwrap());
        if label == "updates" {
            assert!(n < o, "{}: an update is newer than what it updates", p["id"]);
        }
        if label == "contradicts" && n == o {
            same_age_contradicts += 1;
        }
    }
    for label in ["duplicate", "updates", "contradicts", "unrelated"] {
        for (lang, min) in [("th", 15), ("en", 15), ("mixed", 10), ("code", 10)] {
            let n = per.get(&(label.to_string(), lang.to_string())).copied().unwrap_or(0);
            assert!(n >= min, "{label}/{lang}: {n} < {min}");
        }
    }
    for k in alice_core::model::Kind::ALL {
        assert!(kinds.get(k.as_str()).copied().unwrap_or(0) >= 6, "kind {} under-represented", k.as_str());
    }
    assert!(same_age_contradicts >= 15);
    assert_eq!(redactor_hits, 0, "the set must contain nothing secret-shaped");
    let queries = include_str!("fixtures/judge_eval_queries.jsonl").lines().filter(|l| !l.trim().is_empty()).count();
    assert!(queries >= 40);
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-daemon --test judge_eval_fixture` — Expected: pass once the set is complete (it fails with a precise message until then — use it while writing the pairs).

- [ ] **Step 6: The evaluation**

`crates/alice-daemon/tests/jev_eval.rs` (ignored; needs the real model and the key):

```rust
//! Jev vs heuristic on the labelled set, and Jev-graded nDCG@10 of recall. Network, real model,
//! hard budget $2 for the whole run.
//!   set -a; . ./.env; set +a
//!   ALICE_MODELS_DIR=$HOME/.alice/models cargo test -p alice-daemon --release --test jev_eval -- --ignored --nocapture
//!   unset TYPESAFE_API_KEY
use alice_core::embed::{Embedder, cosine, memory_embed_text};
use alice_core::judge::heuristic::{HeuristicJudge, kind_rules};
use alice_core::judge::jev::{JevConfig, JevJudge, MemoryJevStore};
use alice_core::judge::metrics::*;
use alice_core::judge::{Distribution, Judge, JudgeError, JudgeMemory, RELEVANCE_ANSWERS, RELEVANCE_PARTIAL};
use alice_core::model::Kind;
use alice_core::search::{Lists, SearchParams, fusion::Weights, run};
use std::sync::Arc;
use std::time::Duration;

const LABELS: [&str; 4] = ["duplicate", "updates", "contradicts", "unrelated"];
const BUDGET_USD: f64 = 2.0;

fn mem(id: &str, v: &serde_json::Value, now: chrono::DateTime<chrono::Utc>, sim: Option<f32>) -> JudgeMemory {
    JudgeMemory {
        id: id.into(),
        project: "eval".into(),
        kind: Kind::parse(v["kind"].as_str().unwrap()).unwrap(),
        title: v["title"].as_str().unwrap().into(),
        body: v["body"].as_str().unwrap().into(),
        importance: v["importance"].as_u64().unwrap_or(3) as u8,
        created_at: (now - chrono::Duration::days(v["days_ago"].as_i64().unwrap())).to_rfc3339(),
        similarity: sim,
    }
}

struct Run {
    gold: Vec<String>,
    pred: Vec<String>,
    probs: Vec<Distribution>,
}

impl Run {
    fn new() -> Self { Self { gold: vec![], pred: vec![], probs: vec![] } }
    fn push(&mut self, gold: &str, d: Distribution) {
        self.gold.push(gold.into());
        self.pred.push(d.best().map(|b| b.0).unwrap_or_else(|| "unrelated".into()));
        self.probs.push(d);
    }
    /// Auto-apply at 0.85 for non-`unrelated` predictions: (coverage, precision).
    fn auto(&self) -> (f64, f64) {
        let applied: Vec<usize> = (0..self.gold.len())
            .filter(|&i| self.pred[i] != "unrelated" && self.probs[i].p(&self.pred[i]) >= 0.85)
            .collect();
        let right = applied.iter().filter(|&&i| self.pred[i] == self.gold[i]).count();
        let actionable = self.gold.iter().filter(|g| *g != "unrelated").count().max(1);
        (applied.len() as f64 / actionable as f64, if applied.is_empty() { 0.0 } else { right as f64 / applied.len() as f64 })
    }
    fn row(&self, name: &str) -> String {
        let (cov, prec) = self.auto();
        format!("| {name} | {} | {:.3} | {:.3} | {:.3} | {:.3} | {:.0}% | {:.0}% |",
            self.gold.len(), accuracy(&self.gold, &self.pred), macro_f1(&self.gold, &self.pred, &LABELS),
            brier(&self.gold, &self.probs, &LABELS), ece(&self.gold, &self.probs, 10), cov * 100.0, prec * 100.0)
    }
}

#[test]
#[ignore = "network and real model: TYPESAFE_API_KEY and ALICE_MODELS_DIR"]
fn jev_versus_heuristic_and_graded_search() {
    let key = std::env::var("TYPESAFE_API_KEY").expect("set -a; . ./.env; set +a");
    let models = std::path::PathBuf::from(std::env::var("ALICE_MODELS_DIR").expect("ALICE_MODELS_DIR"));
    let e = alice_embed::OnnxEmbedder::new(&models, alice_embed::OnnxOptions::default()).unwrap();
    let now = chrono::Utc::now();
    let store = Arc::new(MemoryJevStore::default());
    let jev = JevJudge::new(
        JevConfig {
            endpoint: "https://api.typesafe.ai/v1/systemone".into(), model: "jev-1.13.0".into(), api_key: key,
            rate_per_sec: 8.0, timeout: Duration::from_secs(30), daily_budget_usd: BUDGET_USD, max_retries: 3, cloud_denied: vec![],
        },
        Arc::new(alice_daemon::judge_http::ReqwestTransport::new().unwrap()),
        store.clone(),
        Arc::new(alice_core::redact::Redactor::builtin()),
    );
    let heuristic = HeuristicJudge::default();
    let pairs: Vec<serde_json::Value> = include_str!("fixtures/judge_eval_pairs.jsonl")
        .lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).unwrap()).collect();

    let (mut h, mut j) = (Run::new(), Run::new());
    let (mut agree, mut asked) = (0usize, 0usize);
    let (mut kind_gold, mut kind_h, mut kind_j) = (vec![], vec![], vec![]);
    let mut stopped: Option<String> = None;
    let mut per_lang: std::collections::BTreeMap<String, (Run, Run)> = Default::default();
    for p in &pairs {
        let new = mem("N", &p["new"], now, None);
        let texts = [memory_embed_text(&new.title, &new.body), memory_embed_text(p["old"]["title"].as_str().unwrap(), p["old"]["body"].as_str().unwrap())];
        let v = e.embed_passages(&texts).unwrap();
        let old = mem("O", &p["old"], now, Some(cosine(&v[0], &v[1])));
        let label = p["label"].as_str().unwrap();
        let lang = p["lang"].as_str().unwrap().to_string();
        let hd = heuristic.compare(&new, std::slice::from_ref(&old)).unwrap().remove(0).probs;
        h.push(label, hd.clone());
        per_lang.entry(lang.clone()).or_insert_with(|| (Run::new(), Run::new())).0.push(label, hd);
        kind_gold.push(p["new_kind"].as_str().unwrap().to_string());
        kind_h.push(kind_rules(&new.title, &new.body).best().unwrap().0);
        if stopped.is_some() { continue; }
        match jev.compare(&new, std::slice::from_ref(&old)) {
            Ok(mut r) => {
                let r = r.remove(0);
                asked += 1;
                agree += usize::from(r.agreement == Some(true));
                j.push(label, r.probs.clone());
                per_lang.get_mut(&lang).unwrap().1.push(label, r.probs);
            }
            Err(JudgeError::Budget(m)) => stopped = Some(m),
            Err(err) => panic!("{}: {err}", p["id"]),
        }
        match jev.classify_kind(&new) {
            Ok(d) => kind_j.push(d.best().unwrap().0),
            Err(JudgeError::Budget(m)) => stopped = Some(m),
            Err(err) => panic!("{}: {err}", p["id"]),
        }
    }

    // Graded search: corpus = every memory of the set; three list modes plus vector-only without the floor.
    let d = tempfile::tempdir().unwrap();
    let c = alice_core::store::open_writer(&d.path().join("a.db")).unwrap();
    let proj = alice_core::store::projects::upsert_project(&c, &alice_core::project::ProjectRef {
        key: "eval".into(), slug: "eval".into(), name: "eval".into(), root: None, remote: None }).unwrap();
    let idx = alice_core::vector::VectorIndex::for_model(e.model_id(), e.dim()).unwrap();
    let mut by_seq: std::collections::HashMap<String, JudgeMemory> = Default::default();
    for (i, p) in pairs.iter().enumerate() {
        for side in ["new", "old"] {
            let m = mem(&format!("{i}-{side}"), &p[side], now, None);
            let prepared = alice_core::store::memories::prepare(alice_core::store::memories::NewMemoryInput {
                content: m.body.clone(), title: Some(m.title.clone()), kind: Some(m.kind),
                created_at: Some(m.created_at.clone()), ..Default::default() }, &proj, format!("{:013}-0000-e", i)).unwrap();
            if let alice_core::store::memories::InsertOutcome::New { id, seq } = alice_core::store::memories::insert(&c, &prepared).unwrap() {
                let v = e.embed_passages(&[memory_embed_text(&m.title, &m.body)]).unwrap().remove(0);
                alice_core::store::embeddings::put(&c, alice_core::store::embeddings::VecKey::Memory(seq), e.model_id(), &v).unwrap();
                idx.upsert(alice_core::store::embeddings::VecKey::Memory(seq), &v).unwrap();
                by_seq.insert(id, m);
            }
        }
    }
    let queries: Vec<serde_json::Value> = include_str!("fixtures/judge_eval_queries.jsonl")
        .lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).unwrap()).collect();
    let modes = [("hybrid", Lists::Hybrid, 0.82f32), ("lexical only", Lists::LexicalOnly, 0.82), ("vector only", Lists::VectorOnly, 0.82), ("vector only, no floor", Lists::VectorOnly, 0.0)];
    let mut ndcg: Vec<Vec<f64>> = vec![vec![]; modes.len()];
    let mut grades_cache: std::collections::HashMap<(String, String), f64> = Default::default();
    for q in &queries {
        if stopped.is_some() { break; }
        let query = q["q"].as_str().unwrap();
        let rankings: Vec<Vec<String>> = modes.iter().map(|(_, lists, floor)| {
            let params = SearchParams { query: query.into(), current_project_id: Some(proj.id.clone()), limit: 10,
                relevance_floor: *floor, raw_min_results: 0, lists: *lists, ..Default::default() };
            run(&c, Some(&idx), &e, &Weights::default(), &params, now).unwrap().hits.into_iter().map(|h| h.id).collect()
        }).collect();
        let pool: Vec<String> = { let mut s: Vec<String> = rankings.concat(); s.sort(); s.dedup(); s };
        let todo: Vec<&String> = pool.iter().filter(|id| !grades_cache.contains_key(&(query.to_string(), (*id).clone()))).collect();
        for batch in todo.chunks(10) {
            let ms: Vec<JudgeMemory> = batch.iter().map(|id| by_seq[*id].clone()).collect();
            match jev.grade_relevance(query, &ms) {
                Ok(ds) => for (id, d) in batch.iter().zip(ds) {
                    grades_cache.insert((query.into(), (*id).clone()), 2.0 * d.p(RELEVANCE_ANSWERS) + d.p(RELEVANCE_PARTIAL));
                },
                Err(JudgeError::Budget(m)) => { stopped = Some(m); break; }
                Err(err) => panic!("{query}: {err}"),
            }
        }
        if stopped.is_some() { break; }
        let pool_grades: Vec<f64> = pool.iter().map(|id| grades_cache[&(query.to_string(), id.clone())]).collect();
        for (m, ranking) in rankings.iter().enumerate() {
            let g: Vec<f64> = ranking.iter().map(|id| grades_cache[&(query.to_string(), id.clone())]).collect();
            ndcg[m].push(ndcg_at(&g, &pool_grades, 10));
        }
    }

    let calls = store.calls();
    let usd: f64 = calls.iter().map(|c| c.usd).sum();
    let mut lat: Vec<f64> = calls.iter().map(|c| c.latency_ms as f64).collect();
    let decisions = j.gold.len() + kind_j.len() + grades_cache.len();
    let mut report = String::new();
    report.push_str("| Judge | Pairs | Accuracy | Macro-F1 | Brier | ECE | Auto-apply coverage | Auto-apply precision |\n|---|---|---|---|---|---|---|---|\n");
    report.push_str(&format!("{}\n{}\n", h.row("heuristic"), j.row("jev-1.13.0")));
    report.push_str("\n| Language | Heuristic accuracy | Jev accuracy |\n|---|---|---|\n");
    for (lang, (hh, jj)) in &per_lang {
        report.push_str(&format!("| {lang} | {:.3} | {:.3} |\n", accuracy(&hh.gold, &hh.pred), accuracy(&jj.gold, &jj.pred)));
    }
    report.push_str(&format!("\nForward/reversed agreement (Jev): {:.1}% of {asked} pairs\n", 100.0 * agree as f64 / asked.max(1) as f64));
    report.push_str(&format!("Kind accuracy: heuristic {:.3}, Jev {:.3} ({} memories)\n",
        accuracy(&kind_gold, &kind_h), accuracy(&kind_gold[..kind_j.len()], &kind_j), kind_gold.len()));
    report.push_str("\n| Recall mode | Queries | nDCG@10 (Jev-graded) |\n|---|---|---|\n");
    for (m, (name, _, _)) in modes.iter().enumerate() {
        let n = &ndcg[m];
        report.push_str(&format!("| {name} | {} | {:.3} |\n", n.len(), n.iter().sum::<f64>() / n.len().max(1) as f64));
    }
    report.push_str(&format!("\nJev requests {}, latency p50 {:.0} ms, p95 {:.0} ms; total ${usd:.4}; ${:.4} per 1,000 decisions ({decisions} decisions)\n",
        calls.len(), percentile(&mut lat.clone(), 0.5), percentile(&mut lat, 0.95), usd * 1000.0 / decisions.max(1) as f64));
    if let Some(m) = &stopped {
        report.push_str(&format!("\n**BUDGET STOP** at ${BUDGET_USD}: {m}. Results above are partial.\n"));
    }
    println!("{report}");
    let out = std::env::var("ALICE_EVAL_OUT").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/m2-jev-eval.md").into());
    std::fs::write(&out, &report).unwrap();
    assert!(usd <= BUDGET_USD + 0.01, "the hard budget held");
}
```

(Adjust helper names to the as-built code: `ProjectRef` fields, `NewMemoryInput.created_at`, `memories::prepare` arguments.)

- [ ] **Step 7: Run it once and write the results**

```bash
cd /Users/icue/Code/AI/project-alice
export S=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2 ALICE_HOME=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2/home; mkdir -p "$ALICE_HOME"; [ -e "$ALICE_HOME/models" ] || ln -s "$HOME/.alice/models" "$ALICE_HOME/models"   # scratch prelude (Global Constraints)
set -a; . ./.env; set +a
ALICE_MODELS_DIR=$HOME/.alice/models nice -n 15 cargo test -j 6 -p alice-daemon --release --test jev_eval -- --ignored --nocapture
unset TYPESAFE_API_KEY
```

Expected: ~600–800 Jev requests at ≤ 8/s (a few minutes), total cost of cents (far under $2), the tables printed and saved to `target/m2-jev-eval.md`. Never paste the key or the `.env` content anywhere.

Create `docs/benchmarks/m2-jev.md`: date, machine, model ids (`jev-1.13.0`, `multilingual-e5-small-q8`), the command lines (with `set -a; . ./.env; set +a`, never the key), the set's composition table, the printed tables, and a short reading of the results: where Jev beats the heuristic (contradictions and paraphrased duplicates are the heuristic's blind spots by construction), whether Jev's probabilities are calibrated enough for `auto_apply_threshold = 0.85` (auto-apply precision should be ≥ 95%; if it is not, say which threshold reaches it — the setting stays 0.85 unless the owner changes it), forward/reversed agreement, which recall mode scores best and by how much, and the cost per 1,000 decisions. State plainly that the set is synthetic and written for this evaluation.

- [ ] **Step 8: Commit**

```bash
nice -n 15 cargo fmt --all && nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
nice -n 15 cargo test -j 6 --workspace
git add crates docs/benchmarks/m2-jev.md
git commit -m "test: Jev evaluation on a 200-pair multilingual set, metrics, docs/benchmarks/m2-jev.md

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 11: Review inbox — store, resolutions, CLI, HTTP, admin MCP tools, briefing line

Spec §5.4: review items have a type (`supersede`, `contradiction`, `merge`, `predicate`, `project`, `sync_conflict`), a payload, a confidence and a status; they are resolved from the UI (SP3), `alice review`, or the admin MCP toolset (`review_list`, `review_resolve`, `merge_memories`, `stats`, off unless `mcp.admin_tools = true`); resolutions are recorded as events. Spec §8.1: `context` reports the pending review count; `get_memory` on a merged duplicate returns the survivor (§5.2). This task builds all of that on Task 4's `review_items` table; Task 12 fills the inbox from the judge.

**Files:**
- Modify: `crates/alice-core/src/store/reviews.rs` (list, get, choices, subjects, `mark_resolved`)
- Modify: `crates/alice-core/src/store/memories.rs` (`merge_into`, `move_to_project`), `crates/alice-core/src/store/kg.rs` (`map_predicate`), `crates/alice-core/src/store/history.rs` (`force_undo`)
- Modify: `crates/alice-core/src/context.rs` (review line), `crates/alice-core/src/api.rs` (`ResolveRequest`, `ResolveResponse`, `MergeRequest`, `MergeResponse`, `Stats`, `Redirect`, `GetResponse.redirects`)
- Modify: `crates/alice-daemon/src/writer.rs` (`WriteOp::{ResolveReview, Merge}`), `service.rs` (`reviews`, `review`, `resolve_review`, `merge`, `stats`, redirect in `get`), `http.rs`, `client.rs`, `mcp.rs` (admin router), `crates/alice/src/{main.rs,cmd.rs}` (`alice review`)
- Test: store tests, writer tests, `tests/http_api.rs`, `tests/mcp.rs`, `crates/alice/tests/cli.rs`

**Interfaces:**
- Consumes: Task 4 (`reviews::{ReviewType, ReviewStatus, NewReview, create, count_open}`, `events`, `history`), Task 6 (`Service::settings`).
- Produces:
  - `alice_core::store::reviews::{ReviewItem { id, review_type, payload, confidence, status, created_at, resolved_at, resolved_by, resolution, subjects: Vec<SubjectRef>, choices: Vec<ReviewChoice> }, SubjectRef { id, title, status }, ReviewChoice { id, description }, ReviewQuery { status, review_type, limit, before }, list, get, choices(ReviewType) -> Vec<ReviewChoice>, mark_resolved(conn, id, choice, by: &Actor) -> Result<()>}`
  - Payload shapes (Task 12 and Task 14 create them): `supersede {new_id, old_id, p, provider}`, `contradiction {new_id, old_id, p, provider}`, `merge {survivor, others: [ids], p, provider}`, `predicate {predicate, canonical, p, provider}`, `project {memory_id, from, to, p, provider}`, `undo {event_id, target_type, target_id, reason}`, `sync_conflict {…}` (M4).
  - Choices: supersede `accept`/`reject`; contradiction `keep_new`/`keep_old`/`keep_both`; merge `merge`/`reject`; predicate `map`/`keep`; project `move`/`keep`; undo `apply`/`dismiss`; sync_conflict `keep_local`/`keep_remote` (refused until M4).
  - `alice_core::store::memories::{merge_into(conn, survivor_id: &str, others: &[String], hlc: &str) -> Result<Vec<String>>, move_to_project(conn, id: &str, project_id: &str, hlc: &str) -> Result<MoveOutcome>}`; `MoveOutcome::{Moved, DuplicateOf(String)}`
  - `alice_core::store::kg::map_predicate(conn, name: &str, canonical: &str, hlc: &str) -> Result<usize>` (relations renamed)
  - `alice_core::store::history::force_undo(conn, event_id, actor, hlc) -> Result<UndoOutcome>`
  - `WriteOp::{ResolveReview { id: String, choice: String, actor: Actor }, Merge { survivor: String, others: Vec<String>, actor: Actor }}`; `WriteOutcome::{Resolved(ReviewItem), Merged(Vec<String>)}`
  - `Service::{reviews(ReviewQuery) -> Result<Vec<ReviewItem>>, review(id) -> Result<ReviewItem>, resolve_review(ResolveRequest) -> Result<ResolveResponse>, merge(MergeRequest) -> Result<MergeResponse>, stats() -> Result<Stats>}`
  - `alice_core::api::{ResolveRequest { id, choice, author }, ResolveResponse { review: ReviewItem, message }, MergeRequest { survivor, others, author }, MergeResponse { merged: Vec<String> }, Stats { memories_by_status: BTreeMap<String, u64>, memories_by_kind: BTreeMap<String, u64>, top_projects: Vec<(String, u64)>, chunks: u64, reviews_open: u64, judge_provider: String, judge_backlog: u64, judge_retry: u64, embed_backlog: u64, cost_today_usd: f64, cost_7d_usd: f64 }, Redirect { from, to }}`; `GetResponse.redirects: Vec<Redirect>`
  - HTTP: `GET /api/v1/reviews`, `GET /api/v1/reviews/{id}`, `POST /api/v1/reviews/{id}/resolve` (`{"choice": …}`), `POST /api/v1/memories/merge`, `GET /api/v1/stats`. Client methods of the same names.
  - MCP admin tools `review_list`, `review_resolve`, `merge_memories`, `stats` (only when `mcp.admin_tools`).
  - CLI: `alice review [list] [--type T] [--all]`, `alice review show <id>`, `alice review resolve <id> <choice>`, `alice stats`.

- [ ] **Step 1: Read the files this task modifies, as they are now.**

- [ ] **Step 2: Write the failing store tests**

In `store/reviews.rs` tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::events::Actor;

    fn db() -> (tempfile::TempDir, rusqlite::Connection) {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        (d, c)
    }

    #[test]
    fn items_list_newest_first_dedupe_while_open_and_resolve_once() {
        let (_d, c) = db();
        let item = |k: &str| NewReview { kind: ReviewType::Supersede, payload: serde_json::json!({"new_id": k, "old_id": "O", "p": 0.7, "provider": "heuristic"}), confidence: Some(0.7), dedupe_key: Some(format!("supersede:{k}:O")) };
        let a = create(&c, &item("A")).unwrap().unwrap();
        assert_eq!(create(&c, &item("A")).unwrap(), None, "same question while open");
        let b = create(&c, &item("B")).unwrap().unwrap();
        let open = list(&c, &ReviewQuery::default()).unwrap();
        assert_eq!(open.iter().map(|i| i.id.clone()).collect::<Vec<_>>(), vec![b.clone(), a.clone()]);
        assert_eq!(open[0].choices.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec!["accept", "reject"]);
        mark_resolved(&c, &a, "reject", &Actor::User).unwrap();
        let err = mark_resolved(&c, &a, "accept", &Actor::User).unwrap_err().to_string();
        assert!(err.contains("already resolved"), "{err}");
        assert!(mark_resolved(&c, &b, "maybe", &Actor::User).unwrap_err().to_string().contains("accept, reject"));
        let got = get(&c, &a).unwrap().unwrap();
        assert_eq!((got.status, got.resolution.as_deref(), got.resolved_by.as_deref()), (ReviewStatus::Resolved, Some("reject"), Some("user")));
        assert!(create(&c, &item("A")).unwrap().is_some(), "the question may be asked again once resolved");
        assert_eq!(count_open(&c).unwrap(), 2);
    }
}
```

In `store/memories.rs` tests (use the module's `setup()`):

```rust
    #[test]
    fn merge_trashes_the_others_with_links_and_moves_provenance() {
        let (_d, c, p) = setup();
        let ins = |t: &str| match insert(&c, &prepare(input(t), &p, "h".into()).unwrap()).unwrap() {
            InsertOutcome::New { id, .. } => id,
            o => panic!("{o:?}"),
        };
        let (s, a, b) = (ins("survivor text"), ins("dup one"), ins("dup two"));
        c.execute("INSERT INTO provenance (memory_id, chunk_id) VALUES (?1, 'CH1')", [&a]).unwrap();
        let merged = merge_into(&c, &s, &[a.clone(), b.clone(), s.clone()], "h2").unwrap();
        assert_eq!(merged, vec![a.clone(), b.clone()], "the survivor is never merged into itself");
        for id in [&a, &b] {
            let st: String = c.query_row("SELECT status FROM memories WHERE id = ?1", [id], |r| r.get(0)).unwrap();
            assert_eq!(st, "trashed");
            let n: i64 = c.query_row("SELECT count(*) FROM links WHERE from_id = ?1 AND to_id = ?2 AND type = 'duplicate_of'", [id, &s], |r| r.get(0)).unwrap();
            assert_eq!(n, 1);
        }
        let prov: i64 = c.query_row("SELECT count(*) FROM provenance WHERE memory_id = ?1 AND chunk_id = 'CH1'", [&s], |r| r.get(0)).unwrap();
        assert_eq!(prov, 1);
    }

    #[test]
    fn moving_to_a_project_that_holds_the_same_text_reports_the_duplicate() {
        let (_d, c, p) = setup();
        let other = crate::store::projects::upsert_project(&c, &crate::project::ProjectRef {
            key: "k2".into(), slug: "other".into(), name: "other".into(), root: None, remote: None }).unwrap();
        let m1 = prepare(input("same text"), &p, "h".into()).unwrap();
        let InsertOutcome::New { id: a, .. } = insert(&c, &m1).unwrap() else { panic!() };
        let m2 = prepare(input("same text"), &other, "h".into()).unwrap();
        let InsertOutcome::New { id: b, .. } = insert(&c, &m2).unwrap() else { panic!() };
        assert_eq!(move_to_project(&c, &a, &other.id, "h3").unwrap(), MoveOutcome::DuplicateOf(b));
        let m3 = prepare(input("unique text"), &p, "h".into()).unwrap();
        let InsertOutcome::New { id: u, .. } = insert(&c, &m3).unwrap() else { panic!() };
        assert_eq!(move_to_project(&c, &u, &other.id, "h4").unwrap(), MoveOutcome::Moved);
        let slug: String = c.query_row("SELECT p.slug FROM memories m JOIN projects p ON p.id = m.project_id WHERE m.id = ?1", [&u], |r| r.get(0)).unwrap();
        assert_eq!(slug, "other");
    }
```

In `store/kg.rs` tests:

```rust
    #[test]
    fn mapping_a_predicate_renames_its_relations_and_future_asserts() {
        let (_d, c) = conn(); // kg.rs's M1 test helper
        for (p, o) in [("uses", "SQLite"), ("use", "Rust")] {
            assert_fact(&c, &AssertFact { subject: "alice".into(), predicate: p.into(), object: o.into(), ..Default::default() }, "h".into()).unwrap();
        }
        assert_eq!(map_predicate(&c, "uses", "use", "h2").unwrap(), 1);
        let facts = query_facts(&c, &FactsQuery { entity: "alice".into(), ..Default::default() }).unwrap();
        assert!(facts.iter().all(|f| f.predicate == "use"), "{facts:?}");
        let out = assert_fact(&c, &AssertFact { subject: "alice".into(), predicate: "uses".into(), object: "Tokio".into(), ..Default::default() }, "h3".into()).unwrap();
        assert_eq!(out.predicate, "use");
        assert!(map_predicate(&c, "uses", "uses", "h4").is_err(), "a predicate cannot map to itself");
    }
```

Run: `nice -n 15 cargo test -j 6 -p alice-core store::` — Expected: FAIL to compile.

- [ ] **Step 3: Implement the store functions**

`store/reviews.rs` additions:

```rust
use crate::error::AliceError;
use crate::store::events::Actor;
use rusqlite::OptionalExtension;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewChoice {
    pub id: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SubjectRef {
    pub id: String,
    pub title: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReviewItem {
    pub id: String,
    pub review_type: ReviewType,
    pub payload: serde_json::Value,
    pub confidence: Option<f64>,
    pub status: ReviewStatus,
    pub created_at: String,
    pub resolved_at: Option<String>,
    pub resolved_by: Option<String>,
    pub resolution: Option<String>,
    /// Memories the item is about (titles, so a person can decide without opening them).
    pub subjects: Vec<SubjectRef>,
    pub choices: Vec<ReviewChoice>,
}

/// Open items by default, newest first. `before` is a review id (paging).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ReviewQuery {
    #[serde(default)]
    pub status: Option<ReviewStatus>,
    #[serde(default)]
    pub review_type: Option<ReviewType>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub before: Option<String>,
}

pub fn choices(t: ReviewType) -> Vec<ReviewChoice> {
    let c = |id: &str, d: &str| ReviewChoice { id: id.into(), description: d.into() };
    match t {
        ReviewType::Supersede => vec![c("accept", "the new memory replaces the old one"), c("reject", "keep both as they are")],
        ReviewType::Contradiction => vec![
            c("keep_new", "the new memory is right; it supersedes the old one"),
            c("keep_old", "the old memory is right; the new one becomes superseded"),
            c("keep_both", "both stay, linked as contradicting"),
        ],
        ReviewType::Merge => vec![c("merge", "keep the survivor; the others become its duplicates"), c("reject", "keep them apart")],
        ReviewType::Predicate => vec![c("map", "treat the new predicate as the existing one"), c("keep", "keep both predicates")],
        ReviewType::Project => vec![c("move", "move the memory to the suggested project"), c("keep", "leave it where it is")],
        ReviewType::Undo => vec![c("apply", "undo anyway: restore the earlier state over the later change"), c("dismiss", "keep the current state")],
        ReviewType::SyncConflict => vec![c("keep_local", "keep this device's version"), c("keep_remote", "take the other device's version")],
    }
}

fn subject_ids(t: ReviewType, p: &serde_json::Value) -> Vec<String> {
    let s = |k: &str| p.get(k).and_then(|v| v.as_str()).map(String::from);
    let mut ids: Vec<String> = match t {
        ReviewType::Supersede | ReviewType::Contradiction => [s("new_id"), s("old_id")].into_iter().flatten().collect(),
        ReviewType::Merge => s("survivor").into_iter()
            .chain(p["others"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)))
            .collect(),
        ReviewType::Project => s("memory_id").into_iter().collect(),
        ReviewType::Undo => [s("target_id")].into_iter().flatten().filter(|_| s("target_type").as_deref() == Some("memory")).collect(),
        ReviewType::Predicate | ReviewType::SyncConflict => vec![],
    };
    ids.dedup();
    ids
}

const COLS: &str = "id, type, payload_json, confidence, status, created_at, resolved_at, resolved_by, resolution";

fn read(conn: &Connection, r: &rusqlite::Row<'_>) -> rusqlite::Result<ReviewItem> {
    let t: String = r.get(1)?;
    let review_type = ReviewType::parse(&t).unwrap_or(ReviewType::Undo);
    let payload: serde_json::Value = serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default();
    let status: String = r.get(4)?;
    let subjects = subject_ids(review_type, &payload)
        .into_iter()
        .filter_map(|id| conn.query_row("SELECT id, title, status FROM memories WHERE id = ?1", [&id], |m| Ok(SubjectRef {
            id: m.get(0)?, title: m.get(1)?, status: m.get(2)?,
        })).optional().ok().flatten())
        .collect();
    Ok(ReviewItem {
        id: r.get(0)?,
        review_type,
        payload,
        confidence: r.get(3)?,
        status: if status == "open" { ReviewStatus::Open } else { ReviewStatus::Resolved },
        created_at: r.get(5)?,
        resolved_at: r.get(6)?,
        resolved_by: r.get(7)?,
        resolution: r.get(8)?,
        subjects,
        choices: choices(review_type),
    })
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<ReviewItem>> {
    Ok(conn.query_row(&format!("SELECT {COLS} FROM review_items WHERE id = ?1"), [id], |r| read(conn, r)).optional()?)
}

pub fn list(conn: &Connection, q: &ReviewQuery) -> Result<Vec<ReviewItem>> {
    let status = match q.status.unwrap_or(ReviewStatus::Open) { ReviewStatus::Open => "open", ReviewStatus::Resolved => "resolved" };
    let before: Option<i64> = match &q.before {
        Some(id) => conn.query_row("SELECT rowid FROM review_items WHERE id = ?1", [id], |r| r.get(0)).optional()?,
        None => None,
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM review_items WHERE status = ?1 AND (?2 IS NULL OR type = ?2) AND (?3 IS NULL OR rowid < ?3)
         ORDER BY rowid DESC LIMIT ?4"
    ))?;
    let rows = stmt.query_map(
        params![status, q.review_type.map(|t| t.as_str()), before, q.limit.unwrap_or(50).clamp(1, 500) as i64],
        |r| read(conn, r),
    )?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Record a resolution. Fails for an unknown or closed item, or a choice the type does not offer.
pub fn mark_resolved(conn: &Connection, id: &str, choice: &str, by: &Actor) -> Result<()> {
    let item = get(conn, id)?.ok_or_else(|| AliceError::NotFound(format!("review item {id}; list them with `alice review`")))?;
    if item.status != ReviewStatus::Open {
        return Err(AliceError::invalid(format!(
            "review item {id} is already resolved ({}); nothing to do", item.resolution.unwrap_or_default()
        )));
    }
    if !item.choices.iter().any(|c| c.id == choice) {
        return Err(AliceError::invalid(format!(
            "\"{choice}\" is not a choice for this {} item; use one of: {}",
            item.review_type.as_str(),
            item.choices.iter().map(|c| c.id.as_str()).collect::<Vec<_>>().join(", ")
        )));
    }
    conn.execute(
        "UPDATE review_items SET status = 'resolved', resolved_at = ?1, resolved_by = ?2, resolution = ?3 WHERE id = ?4",
        params![now_rfc3339(), by.as_string(), choice, id],
    )?;
    Ok(())
}
```

`store/memories.rs` additions:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveOutcome {
    Moved,
    /// The target project already holds this text in a live memory (its id).
    DuplicateOf(String),
}

/// `others` become trashed duplicates of `survivor`: each gets a `duplicate_of` link to it and
/// the survivor gains their provenance. The survivor itself and unknown ids are skipped.
pub fn merge_into(conn: &Connection, survivor_id: &str, others: &[String], hlc: &str) -> Result<Vec<String>> {
    let survivor_seq = seq_of(conn, survivor_id)?.ok_or_else(|| AliceError::NotFound(format!("memory {survivor_id} (survivor)")))?;
    let survivor: String = conn.query_row("SELECT id FROM memories WHERE seq = ?1", [survivor_seq], |r| r.get(0))?;
    let mut merged = Vec::new();
    for o in others {
        let Some(seq) = seq_of(conn, o)?.filter(|s| *s != survivor_seq) else { continue };
        let id: String = conn.query_row("SELECT id FROM memories WHERE seq = ?1", [seq], |r| r.get(0))?;
        set_status(conn, seq, Status::Trashed, Some(hlc))?;
        add_link(conn, &id, &survivor, "duplicate_of", "merge")?;
        conn.execute(
            "INSERT OR IGNORE INTO provenance (memory_id, chunk_id) SELECT ?1, chunk_id FROM provenance WHERE memory_id = ?2",
            params![survivor, id],
        )?;
        merged.push(id);
    }
    Ok(merged)
}

pub fn move_to_project(conn: &Connection, id: &str, project_id: &str, hlc: &str) -> Result<MoveOutcome> {
    let seq = seq_of(conn, id)?.ok_or_else(|| AliceError::NotFound(format!("memory {id}")))?;
    let (kind, title, body): (String, String, String) =
        conn.query_row("SELECT kind, title, body FROM memories WHERE seq = ?1", [seq], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let slug: String = conn.query_row("SELECT slug FROM projects WHERE id = ?1", [project_id], |r| r.get(0))?;
    let hash = content_hash(&slug, Kind::parse(&kind).unwrap_or(Kind::Note), &title, &body);
    if let Some(other) = conn.query_row(
        "SELECT id FROM memories WHERE project_id = ?1 AND content_hash = ?2 AND status <> 'trashed' AND seq <> ?3",
        params![project_id, hash, seq], |r| r.get::<_, String>(0)).optional()?
    {
        return Ok(MoveOutcome::DuplicateOf(other));
    }
    conn.execute(
        "UPDATE memories SET project_id = ?1, content_hash = ?2, updated_at = ?3, hlc = ?4 WHERE seq = ?5",
        params![project_id, hash, now_rfc3339(), hlc, seq],
    )?;
    Ok(MoveOutcome::Moved)
}
```

`store/kg.rs`:

```rust
/// From now on `name` means `canonical`: existing relations are renamed and later asserts with
/// `name` use `canonical`. Returns how many relations were renamed.
pub fn map_predicate(conn: &Connection, name: &str, canonical: &str, hlc: &str) -> Result<usize> {
    let name = predicate_key(name)?;
    let target = canonical_predicate(conn, &predicate_key(canonical)?)?
        .ok_or_else(|| AliceError::NotFound(format!("predicate {canonical}")))?;
    if name == target {
        return Err(AliceError::invalid(format!("{name} cannot map to itself")));
    }
    conn.execute(
        "INSERT INTO predicates (name, canonical) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET canonical = excluded.canonical",
        params![name, target],
    )?;
    // Names that pointed at `name` follow it.
    conn.execute("UPDATE predicates SET canonical = ?1 WHERE canonical = ?2", params![target, name])?;
    Ok(conn.execute("UPDATE relations SET predicate = ?1, hlc = ?2 WHERE predicate = ?3", params![target, hlc, name])?)
}
```

`store/history.rs` — `force_undo`: the same as `undo` without the "changed since" comparison (still refuses purged targets, non-undoable actions and events already undone); implement by giving the private body a `check: bool` parameter that `undo` calls with `true` and `force_undo` with `false`.

Run: `nice -n 15 cargo test -j 6 -p alice-core store::` — Expected: pass.

- [ ] **Step 4: Resolutions in the writer, redirects and the briefing line**

`writer.rs`:

```rust
    /// Apply a review decision and record it.
    ResolveReview { id: String, choice: String, actor: Actor },
    /// `others` become duplicates of `survivor` (admin tool, review `merge`).
    Merge { survivor: String, others: Vec<String>, actor: Actor },
```

```rust
        WriteOp::Merge { survivor, others, actor } => {
            WriteOutcome::Merged(merge(conn, st, &survivor, &others, &actor, None)?)
        }
        WriteOp::ResolveReview { id, choice, actor } => {
            let item = reviews::get(conn, &id)?.ok_or_else(|| AliceError::NotFound(format!("review item {id}")))?;
            let p = &item.payload;
            let s = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
            match (item.review_type, choice.as_str()) {
                (ReviewType::Supersede, "accept") | (ReviewType::Contradiction, "keep_new") => {
                    supersede(conn, st, &s("old_id"), &s("new_id"), &actor, None)?
                }
                (ReviewType::Contradiction, "keep_old") => supersede(conn, st, &s("new_id"), &s("old_id"), &actor, None)?,
                (ReviewType::Merge, "merge") => {
                    let others: Vec<String> = p["others"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(String::from)).collect();
                    merge(conn, st, &s("survivor"), &others, &actor, None)?;
                }
                (ReviewType::Predicate, "map") => {
                    let h = st.hlc.tick();
                    let n = kg::map_predicate(conn, &s("predicate"), &s("canonical"), &h)?;
                    record(conn, st, EventInput { actor: &actor, action: "predicate.map", target_type: "predicate", target_id: &s("predicate"),
                        before: None, after: Some(serde_json::json!({"canonical": s("canonical"), "relations": n})), group: None })?;
                }
                (ReviewType::Project, "move") => {
                    let target = projects::resolve_slug(conn, &s("to"))?.ok_or_else(|| AliceError::invalid(format!("project {} no longer exists", s("to"))))?;
                    let before = snap(conn, &s("memory_id"))?;
                    let h = st.hlc.tick();
                    match memories::move_to_project(conn, &s("memory_id"), &target.id, &h)? {
                        memories::MoveOutcome::Moved => record(conn, st, EventInput { actor: &actor, action: "memory.move", target_type: "memory",
                            target_id: &s("memory_id"), before, after: snap(conn, &s("memory_id"))?, group: None })?,
                        memories::MoveOutcome::DuplicateOf(other) => { merge(conn, st, &other, &[s("memory_id")], &actor, None)?; }
                    }
                }
                (ReviewType::Undo, "apply") => {
                    let mut tick = || st.hlc.tick();
                    history::force_undo(conn, &s("event_id"), &actor, &mut tick)?;
                }
                (ReviewType::SyncConflict, _) => {
                    return Err(AliceError::invalid("sync conflicts are resolved by `alice sync` (coming in M4)"));
                }
                _ => {} // reject / keep / keep_both / dismiss: nothing changes
            }
            reviews::mark_resolved(conn, &id, &choice, &actor)?;
            record(conn, st, EventInput { actor: &actor, action: "review.resolve", target_type: "review", target_id: &id,
                before: None, after: Some(serde_json::json!({"choice": choice})), group: None })?;
            WriteOutcome::Resolved(reviews::get(conn, &id)?.expect("just resolved"))
        }
```

with helpers next to `apply` (each records its events in one group: `group`, or a new one when `None`; Task 12 passes its judgement's group so one judgement is one undoable group):

```rust
/// `old` becomes superseded by `new` (status and a `supersedes` link), with events.
fn supersede(conn: &Connection, st: &mut State, old: &str, new: &str, actor: &Actor, group: Option<&str>) -> Result<()> {
    let group = group.map(str::to_owned).unwrap_or_else(alice_core::ids::new_id);
    let (b_old, b_new) = (snap(conn, old)?, snap(conn, new)?);
    let h = st.hlc.tick();
    memories::apply_update(conn, old, &MemoryUpdate { action: UpdateAction::Supersede, superseded_by: Some(new.into()), ..Default::default() }, h)?;
    record(conn, st, EventInput { actor, action: "memory.supersede", target_type: "memory", target_id: old, before: b_old, after: snap(conn, old)?, group: Some(&group) })?;
    record(conn, st, EventInput { actor, action: "memory.link", target_type: "memory", target_id: new, before: b_new, after: snap(conn, new)?, group: Some(&group) })?;
    Ok(())
}

fn merge(conn: &Connection, st: &mut State, survivor: &str, others: &[String], actor: &Actor, group: Option<&str>) -> Result<Vec<String>> {
    let group = group.map(str::to_owned).unwrap_or_else(alice_core::ids::new_id);
    let befores: Vec<(String, Option<serde_json::Value>)> = others.iter().map(|o| Ok((o.clone(), snap(conn, o)?))).collect::<Result<_>>()?;
    let b_survivor = snap(conn, survivor)?;
    let h = st.hlc.tick();
    let merged = memories::merge_into(conn, survivor, others, &h)?;
    for (id, before) in befores.into_iter().filter(|(id, _)| merged.contains(id)) {
        record(conn, st, EventInput { actor, action: "memory.merge", target_type: "memory", target_id: &id, before, after: snap(conn, &id)?, group: Some(&group) })?;
    }
    if !merged.is_empty() {
        record(conn, st, EventInput { actor, action: "memory.merge_into", target_type: "memory", target_id: survivor, before: b_survivor, after: snap(conn, survivor)?, group: Some(&group) })?;
    }
    Ok(merged)
}
```

(Provenance moved by a merge is not part of the snapshot; undoing a merge restores status and links, and the copied provenance rows stay on the survivor — harmless and documented in the function's comment.) After `ResolveReview` and whenever `reviews::create` returns `Some(id)` anywhere in the writer, push `StreamItem::Review { id, review_type }` into the batch's broadcast list.

`service.rs` `get`: after `get_many`, for each requested id that resolved to a `trashed` memory with an outgoing `duplicate_of` link, fetch the link target instead, add it to `memories` (once) and push `Redirect { from: requested, to: survivor }` into `GetResponse.redirects` (`#[serde(default, skip_serializing_if = "Vec::is_empty")]`). `context.rs` `build`: when `reviews::count_open(conn)? > 0`, add a final section:

```rust
    let open = crate::store::reviews::count_open(conn)?;
    if open > 0 {
        b.push_section("Review", &format!(
            "{open} item(s) wait for a decision (duplicates, updates or contradictions Alice was not sure about). Run `alice review`, or ask the user."
        ));
    }
```

(Use the builder's existing way of adding a section with plain text; add a small `push_section` if it only knows memory rows. Add a context test: one open review → the briefing ends with the Review section; none → no section.)

- [ ] **Step 5: Service, HTTP, client, CLI**

`service.rs`:

```rust
    pub async fn reviews(&self, q: ReviewQuery) -> Result<Vec<ReviewItem>> {
        self.read(move |c| reviews::list(c, &q)).await
    }
    pub async fn review(&self, id: String) -> Result<ReviewItem> {
        self.read(move |c| reviews::get(c, &id)?.ok_or_else(|| AliceError::NotFound(format!("review item {id}; list them with `alice review`")))).await
    }
    pub async fn resolve_review(&self, req: ResolveRequest) -> Result<ResolveResponse> {
        let actor = Actor::from_author(&req.author);
        match self.writer.submit(WriteOp::ResolveReview { id: req.id, choice: req.choice.clone(), actor }).await? {
            WriteOutcome::Resolved(review) => Ok(ResolveResponse { message: format!("Resolved: {}.", req.choice), review }),
            other => Err(unexpected(other)),
        }
    }
    pub async fn merge(&self, req: MergeRequest) -> Result<MergeResponse> {
        let actor = Actor::from_author(&req.author);
        match self.writer.submit(WriteOp::Merge { survivor: req.survivor, others: req.others, actor }).await? {
            WriteOutcome::Merged(merged) => Ok(MergeResponse { merged }),
            other => Err(unexpected(other)),
        }
    }
    pub async fn stats(&self) -> Result<Stats> {
        let provider = self.judge().provider().to_string();
        self.read(move |c| {
            let group = |sql: &str| -> Result<BTreeMap<String, u64>> {
                let mut stmt = c.prepare(sql)?;
                let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?;
                Ok(rows.collect::<std::result::Result<_, _>>()?)
            };
            let count = |sql: &str| -> Result<u64> { Ok(c.query_row(sql, [], |r| r.get::<_, i64>(0))? as u64) };
            let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
            let week = (chrono::Utc::now() - chrono::Duration::days(6)).format("%Y-%m-%d").to_string();
            let costs = alice_core::store::judge::cost_since(c, &week)?;
            let mut top = c.prepare("SELECT p.slug, count(*) FROM memories m JOIN projects p ON p.id = m.project_id WHERE m.status <> 'trashed' GROUP BY p.slug ORDER BY 2 DESC LIMIT 20")?;
            let top_projects = top.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?.collect::<std::result::Result<_, _>>()?;
            Ok(Stats {
                memories_by_status: group("SELECT status, count(*) FROM memories GROUP BY status")?,
                memories_by_kind: group("SELECT kind, count(*) FROM memories WHERE status <> 'trashed' GROUP BY kind")?,
                top_projects,
                chunks: count("SELECT count(*) FROM chunks")?,
                reviews_open: reviews::count_open(c)?,
                judge_provider: provider,
                judge_backlog: count("SELECT count(*) FROM memories WHERE judge_state = 'pending' AND status <> 'trashed'")?,
                judge_retry: count("SELECT count(*) FROM memories WHERE judge_state = 'retry'")?,
                embed_backlog: alice_core::store::embeddings::backlog(c)? as u64,
                cost_today_usd: costs.iter().filter(|r| r.day == today).map(|r| r.usd).sum(),
                cost_7d_usd: costs.iter().map(|r| r.usd).sum(),
            })
        }).await
    }
```

HTTP routes (guarded): `.route("/api/v1/reviews", get(list_reviews))`, `.route("/api/v1/reviews/{id}", get(get_review))`, `.route("/api/v1/reviews/{id}/resolve", post(resolve_review))` (body `{"choice": …}`; the path id wins), `.route("/api/v1/memories/merge", post(merge))`, `.route("/api/v1/stats", get(stats))`; `list_reviews` takes `Query<ReviewQuery>` (with the same rejection message style as `/events`). Client: `reviews(&ReviewQuery)`, `review(id)`, `resolve_review(&ResolveRequest)`, `merge(&MergeRequest)`, `stats()`.

CLI (`main.rs`): `#[command(subcommand)] Review(Option<cmd::ReviewCmd>)` doc "Decisions Alice left to you: duplicates, updates, contradictions"; `Stats` doc "Counts, review backlog and judge costs". `cmd.rs`:

```rust
#[derive(Subcommand)]
pub enum ReviewCmd {
    /// Open items (or all with --all).
    List {
        #[arg(long = "type")]
        review_type: Option<String>,
        #[arg(long)]
        all: bool,
    },
    Show { id: String },
    /// Apply one of the item's choices.
    Resolve { id: String, choice: String },
}
```

Text output of `list`: one block per item — `<id>  <type>  p=<confidence>` then each subject as `  - <title> [<id>] (<status>)` then `  choices: accept | reject`. `resolve` prints the message. Map an unknown `--type` to an error listing the types.

- [ ] **Step 6: Admin MCP tools**

`mcp.rs`: a second router

```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResolveArgs {
    pub id: String,
    pub choice: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MergeArgs {
    /// The memory that stays.
    pub survivor: String,
    /// Memories that become its duplicates.
    pub others: Vec<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct NoArgs {}

#[tool_router(router = admin_tool_router)]
impl AliceMcp {
    #[tool(description = "List review items Alice could not decide alone (duplicates, updates, contradictions), with their choices.", annotations(read_only_hint = true))]
    async fn review_list(&self, Parameters(q): Parameters<ReviewQuery>) -> Result<CallToolResult, McpError> {
        Ok(match self.backend.reviews(q).await { Ok(r) => json(&r), Err(e) => tool_err(e) })
    }
    #[tool(description = "Resolve a review item with one of its choices.")]
    async fn review_resolve(&self, Parameters(a): Parameters<ResolveArgs>, ctx: RequestContext<RoleServer>) -> Result<CallToolResult, McpError> {
        let author = Author { agent: Self::agent(&ctx), ..Default::default() };
        Ok(match self.backend.resolve_review(ResolveRequest { id: a.id, choice: a.choice, author }).await { Ok(r) => text(r.message), Err(e) => tool_err(e) })
    }
    #[tool(description = "Merge duplicate memories into one survivor; the others are trashed and point to it.")]
    async fn merge_memories(&self, Parameters(a): Parameters<MergeArgs>, ctx: RequestContext<RoleServer>) -> Result<CallToolResult, McpError> {
        let author = Author { agent: Self::agent(&ctx), ..Default::default() };
        Ok(match self.backend.merge(MergeRequest { survivor: a.survivor, others: a.others, author }).await { Ok(r) => json(&r), Err(e) => tool_err(e) })
    }
    #[tool(description = "Memory counts, review backlog, judge backlog and costs.", annotations(read_only_hint = true))]
    async fn stats(&self, Parameters(_): Parameters<NoArgs>) -> Result<CallToolResult, McpError> {
        Ok(match self.backend.stats().await { Ok(r) => json(&r), Err(e) => tool_err(e) })
    }
}
```

`Backend` gains `reviews`, `resolve_review`, `merge`, `stats` (Local → `Service`, Remote → client with `READ_WAIT`). `AliceMcp::new(backend, admin: bool)` builds `tool_router: if admin { Self::tool_router() + Self::admin_tool_router() } else { Self::tool_router() }`. `http_service` builds the server per session: `StreamableHttpService::new(move || Ok(AliceMcp::new(Backend::Local(svc.clone()), svc.settings().mcp.admin_tools)), …)` so a settings change applies to new sessions. `run_stdio` reads `mcp.admin_tools` with Task 5's `crate::config::load_effective(&home).resolved.settings.mcp.admin_tools` (the one "config.toml + `ALICE_*` + defaults" rule) — no daemon round trip, so start-up stays fast.

Test in `crates/alice-daemon/tests/mcp.rs`, with the file's existing helpers (it has no duplex helper; use the HTTP ones): `let (_d, _home, d) = daemon().await;` then `TestClient.serve(http_transport(&d))` lists exactly the 7 core tools; `d.svc.patch_settings(serde_json::Map::from_iter([("mcp.admin_tools".to_string(), serde_json::json!(true))])).unwrap()`, and a **new** `TestClient.serve(http_transport(&d))` session lists the 7 plus the four admin tools; `call_tool(call("stats", serde_json::json!({})))` returns JSON with `reviews_open`; the first session's list is unchanged (settings apply to new sessions).

- [ ] **Step 7: Interface tests and a process round trip**

`tests/http_api.rs` (with Task 4's `Srv` helpers: `post_json`, `get_json`, `post` for the 400 case): create an `undo` review item by editing a memory twice and undoing the first edit (`/api/v1/undo` answers `undone: false` with a `review_id`); `GET /api/v1/reviews` lists it with choices `apply`, `dismiss`; `POST /api/v1/reviews/{id}/resolve {"choice":"apply"}` restores the first text; resolving again answers 400 "already resolved". Merge two memories through `/api/v1/memories/merge`; `/api/v1/get` of the merged id returns the survivor with `redirects`. `GET /api/v1/stats` has `reviews_open: 0` at the end.

`crates/alice/tests/cli.rs`: the same undo-conflict flow through `alice history`, `alice undo`, `alice review list` (shows the item), `alice review resolve <id> dismiss`, `alice --json stats`.

Run: `nice -n 15 cargo test -j 6 --workspace` — Expected: pass; no stray daemons.

- [ ] **Step 8: Commit**

```bash
nice -n 15 cargo fmt --all && nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
git add crates
git commit -m "feat: review inbox with resolutions, merges, stats, admin MCP tools and the briefing's review line

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 12: Relation-to-existing pipeline, kind and project routing, activation after judging

Spec §5.1 step 5: embed → add to the vector index → Judge: compare with the 5 nearest memories in the project → link mentioned entities → set status `active`. Spec §5.2 table: duplicate → the older memory survives and gains the new one's provenance, the new one becomes `trashed` with a `duplicate_of` link and `get_memory` on it returns the survivor (Task 11 did the redirect); updates → new `supersedes` old; contradicts → `contradicts` link + review item; kind is set when the writer did not; project routing when cwd/roots are unknown. Thresholds: auto-apply at P ≥ `judge.auto_apply_threshold`, review item at P ≥ `judge.review_threshold`, else nothing; heuristic results auto-apply only for duplicates; a Judge failure, timeout or exhausted budget falls back to the heuristic and marks `judge_state = 'retry'`; a Judge failure never fails a write.

**Files:**
- Create: `crates/alice-core/src/store/sql/0003_judge.sql`; modify `store/migrations.rs` (version 3)
- Create: `crates/alice-core/src/judge/pipeline.rs` (+ `pub mod pipeline;`)
- Modify: `crates/alice-core/src/store/memories.rs` (`kind_explicit`, `project_defaulted` on insert; `pub fn link`), `crates/alice-core/src/store/judge.rs` (`pending`, `PendingJudgement`, `requeue_retries`), `crates/alice-core/src/settings.rs` (`load_project_denied`)
- Create: `crates/alice-daemon/src/judge_worker.rs`; modify `lib.rs`
- Modify: `crates/alice-daemon/src/writer.rs` (`ProjectChoice::Fallback`, `WriteOp::ApplyJudgement`, no activation in `StoreEmbeddings`/`EmbedFailed`; three M1 writer tests updated, Step 7), `worker.rs` (wake the judge; tests), `service.rs` (spawn the judge worker; `ProjectResolver::choose` returns `Fallback`; Task 7's `project_root` match gains `Fallback`; `Health` fields; test helper `wait_settled`), `crates/alice-core/src/api.rs` (`Health.judge_backlog`, `judge_provider`, `judge_warning`)
- Modify: `crates/alice-daemon/src/judge_http.rs` (test-only scripted Jev transport)
- Test: pipeline unit tests, judge worker tests, service tests

**Interfaces:**
- Consumes: Task 8 (`Judge`, `HeuristicJudge`, `Relation`, `Distribution`, `ProjectOption`, `text::title_similarity`), Task 9 (`JudgeError`, `Service::judge`), Task 11 (`writer::{supersede, merge}`, `memories::move_to_project`, `reviews::create`), Task 4 (`record`, `snap`), Task 7 (`Redactors::privacy_for` for a project's `cloud_denied`).
- Produces:
  - Schema v3: `memories.kind_explicit` (0 when `kind` defaulted to `note`), `memories.project_defaulted` (1 when the project fell back to `global` because nothing told it), `memories.judge_attempts`, index `memories_judge`.
  - `alice_core::judge::pipeline::{Thresholds { auto: f64, review: f64 }, JudgeAction::{DuplicateOf { survivor: String, p: f64 }, Supersedes { old: String, p: f64 }, Contradicts { other: String, p: f64 }}, Decided { actions: Vec<JudgeAction>, reviews: Vec<NewReview> }, decide(new: &JudgeMemory, relations: &[Relation], t: &Thresholds, provider: &str) -> Decided, as_review(new_id: &str, action: &JudgeAction, provider: &str) -> Option<NewReview>}`
  - `alice_core::store::judge::{PendingJudgement { seq, id, stamp, project_id, project_slug, kind_explicit: bool, project_defaulted: bool, attempts: u32 }, pending(conn, limit) -> Result<Vec<PendingJudgement>>, requeue_retries(conn, max_attempts: u32) -> Result<(usize, usize)>, judge_backlog(conn) -> Result<u64>}`
  - `alice_daemon::writer::{ProjectChoice::Fallback, Judgement { seq, id, stamp, provider, actions, reviews, kind: Option<Kind>, project_to: Option<String>, retry: bool, seen: Vec<(String, String)> }, WriteOp::ApplyJudgement(Judgement)}`
  - `alice_daemon::judge_worker::{JudgeWorkerConfig, spawn_judge_worker(cfg) -> JoinHandle<()>, judge_one(…) -> Result<Judgement>}`; `WorkerConfig::judge_wake: Option<Sender<()>>`
  - `Health::{judge_backlog: u64, judge_provider: String, judge_warning: Option<String>}`
  - Event actions by `judge:<provider>`: `memory.merge` (+ `memory.merge_into`), `memory.supersede` (+ `memory.link`), `memory.link` (contradicts), `memory.edit` (kind), `memory.move` (project), all of one judgement in one group.

- [ ] **Step 1: Read the files this task modifies, as they are now.**

- [ ] **Step 2: Migration 3 and the insert flags**

`crates/alice-core/src/store/sql/0003_judge.sql`:

```sql
-- Alice schema v3: what the judge needs per memory (spec §5.1 step 5, §5.2).
ALTER TABLE memories ADD COLUMN kind_explicit INTEGER NOT NULL DEFAULT 1;      -- 0: kind defaulted to note
ALTER TABLE memories ADD COLUMN project_defaulted INTEGER NOT NULL DEFAULT 0;  -- 1: global only because no project was known
ALTER TABLE memories ADD COLUMN judge_attempts INTEGER NOT NULL DEFAULT 0;
CREATE INDEX memories_judge ON memories(judge_state) WHERE judge_state <> 'done';
-- Memories written before the judge existed are not judged after the fact.
UPDATE memories SET judge_state = 'done' WHERE judge_state = 'pending' AND status <> 'pending';
```

`migrations.rs`: add `(3, include_str!("sql/0003_judge.sql"))`, `CURRENT_VERSION = 3`. `NewMemoryInput` gains `pub project_defaulted: bool`; `PreparedMemory` gains `kind_explicit: bool` (= `input.kind.is_some()`) and `project_defaulted: bool`; `insert` writes both columns. Make `add_link` public as `pub fn link(conn, from, to, kind, by) -> Result<()>`. In `writer.rs`:

```rust
pub enum ProjectChoice {
    Slug(String),
    Detected(ProjectRef),
    /// Nothing named a project and there is no working directory: `global`, and the judge may
    /// route the memory to a project later.
    Fallback,
}
```

`resolve(conn, Fallback)` = `projects::resolve_slug(conn, "global")`; the `Remember` op sets `input.project_defaulted = matches!(project, ProjectChoice::Fallback)`. `ProjectResolver::choose(None, None)` returns `Fallback`. Keep every existing `ProjectChoice::Slug("global")` in tests. **Cross-task edit:** the new variant makes Task 7's `Service::project_root` match non-exhaustive — add `ProjectChoice::Fallback => None` there (global has no working tree, so the global privacy settings apply); search `service.rs`, `writer.rs` and `http.rs` for other `match` on `ProjectChoice` and extend them the same way.

Store test (in `memories.rs`): a memory inserted with `kind: None` has `kind_explicit = 0`; with `kind: Some(..)` it is 1; `project_defaulted` is stored as given. Migration test: a v2 database (Task 4's test style) migrates to v3, an M1-era `active` memory gets `judge_state = 'done'`, a `pending` one stays `pending`.

- [ ] **Step 3: Write the failing pipeline tests**

`crates/alice-core/src/judge/pipeline.rs` tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::{Distribution, JudgeMemory, Relation};
    use crate::model::Kind;

    fn new() -> JudgeMemory {
        JudgeMemory { id: "NEW".into(), project: "app".into(), kind: Kind::Fact, title: "t".into(), body: "b".into(),
            importance: 3, created_at: "2026-10-13T00:00:00.000Z".into(), similarity: None }
    }
    fn rel(id: &str, pairs: &[(&str, f64)]) -> Relation {
        Relation { existing_id: id.into(), probs: Distribution::from_pairs(pairs.iter().map(|(k, v)| (k.to_string(), *v))), agreement: None }
    }
    const T: Thresholds = Thresholds { auto: 0.85, review: 0.5 };

    #[test]
    fn a_confident_duplicate_wins_and_stops_everything_else() {
        let d = decide(&new(), &[rel("A", &[("updates", 0.9), ("unrelated", 0.1)]), rel("B", &[("duplicate", 0.97), ("unrelated", 0.03)])], &T, "jev");
        assert_eq!(d.actions, vec![JudgeAction::DuplicateOf { survivor: "B".into(), p: 0.97 }]);
        assert!(d.reviews.is_empty());
        let h = decide(&new(), &[rel("B", &[("duplicate", 0.98), ("unrelated", 0.02)])], &T, "heuristic");
        assert_eq!(h.actions.len(), 1, "duplicates auto-apply for the heuristic too");
    }

    #[test]
    fn updates_apply_for_a_model_judge_but_only_suggest_for_the_heuristic() {
        let r = [rel("OLD", &[("updates", 0.9), ("unrelated", 0.1)])];
        assert_eq!(decide(&new(), &r, &T, "jev").actions, vec![JudgeAction::Supersedes { old: "OLD".into(), p: 0.9 }]);
        let h = decide(&new(), &r, &T, "heuristic");
        assert!(h.actions.is_empty());
        assert_eq!(h.reviews.len(), 1);
        assert_eq!(h.reviews[0].kind, crate::store::reviews::ReviewType::Supersede);
        assert_eq!(h.reviews[0].dedupe_key.as_deref(), Some("supersede:NEW:OLD"));
    }

    #[test]
    fn contradictions_link_and_ask_middle_band_asks_low_band_does_nothing() {
        let d = decide(&new(), &[rel("C", &[("contradicts", 0.8), ("unrelated", 0.2)])], &T, "jev");
        assert_eq!(d.actions, vec![JudgeAction::Contradicts { other: "C".into(), p: 0.8 }]);
        assert_eq!(d.reviews[0].kind, crate::store::reviews::ReviewType::Contradiction);
        let mid = decide(&new(), &[rel("D", &[("duplicate", 0.6), ("unrelated", 0.4)])], &T, "jev");
        assert!(mid.actions.is_empty());
        assert_eq!(mid.reviews[0].kind, crate::store::reviews::ReviewType::Merge);
        assert_eq!(mid.reviews[0].payload["survivor"], "D");
        let low = decide(&new(), &[rel("E", &[("updates", 0.4), ("unrelated", 0.6)])], &T, "jev");
        assert_eq!((low.actions.len(), low.reviews.len()), (0, 0));
    }
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-core judge::pipeline` — Expected: FAIL to compile.

- [ ] **Step 4: Implement `pipeline.rs` and the store helpers**

```rust
//! From relation probabilities to actions and review items (spec §5.2 thresholds).

use super::{JudgeMemory, Relation};
use crate::store::reviews::{NewReview, ReviewType};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    pub auto: f64,
    pub review: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum JudgeAction {
    /// The new memory duplicates `survivor`: it is trashed and points to it.
    DuplicateOf { survivor: String, p: f64 },
    /// The new memory supersedes `old`.
    Supersedes { old: String, p: f64 },
    /// A `contradicts` link (a review item asks which is right).
    Contradicts { other: String, p: f64 },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Decided {
    pub actions: Vec<JudgeAction>,
    pub reviews: Vec<NewReview>,
}

/// The strongest non-`unrelated` relation and its probability.
fn strongest(r: &Relation) -> Option<(&'static str, f64)> {
    ["duplicate", "updates", "contradicts"]
        .into_iter()
        .map(|k| (k, r.probs.p(k)))
        .filter(|(_, p)| *p > 0.0)
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

pub fn decide(new: &JudgeMemory, relations: &[Relation], t: &Thresholds, provider: &str) -> Decided {
    let heuristic = provider == "heuristic";
    let mut out = Decided::default();
    // A confident duplicate settles it: the new memory goes to the trash.
    if let Some((r, p)) = relations
        .iter()
        .filter_map(|r| strongest(r).filter(|(k, p)| *k == "duplicate" && *p >= t.auto).map(|(_, p)| (r, p)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
    {
        out.actions.push(JudgeAction::DuplicateOf { survivor: r.existing_id.clone(), p });
        return out;
    }
    for r in relations {
        let Some((kind, p)) = strongest(r) else { continue };
        if p < t.review {
            continue;
        }
        let old = r.existing_id.clone();
        let review = |kind: ReviewType, payload: serde_json::Value, key: String| NewReview {
            kind, payload, confidence: Some(p), dedupe_key: Some(key),
        };
        match kind {
            "updates" if p >= t.auto && !heuristic => out.actions.push(JudgeAction::Supersedes { old, p }),
            "updates" => out.reviews.push(review(
                ReviewType::Supersede,
                serde_json::json!({"new_id": new.id, "old_id": old, "p": p, "provider": provider}),
                format!("supersede:{}:{old}", new.id),
            )),
            "duplicate" => {
                let mut ids = [new.id.clone(), old.clone()];
                ids.sort();
                out.reviews.push(review(
                    ReviewType::Merge,
                    serde_json::json!({"survivor": old, "others": [new.id], "p": p, "provider": provider}),
                    format!("merge:{}", ids.join(":")),
                ));
            }
            "contradicts" => {
                out.actions.push(JudgeAction::Contradicts { other: old.clone(), p });
                out.reviews.push(review(
                    ReviewType::Contradiction,
                    serde_json::json!({"new_id": new.id, "old_id": old, "p": p, "provider": provider}),
                    format!("contradiction:{}:{old}", new.id),
                ));
            }
            _ => {}
        }
    }
    out
}
```

and, in the same file, the review item an action becomes when its target changed before it could be applied:

```rust
/// The review item that asks about `action` instead of applying it.
pub fn as_review(new_id: &str, action: &JudgeAction, provider: &str) -> Option<NewReview> {
    let (kind, payload, p, key) = match action {
        JudgeAction::Supersedes { old, p } => (ReviewType::Supersede, serde_json::json!({"new_id": new_id, "old_id": old, "p": p, "provider": provider}), *p, format!("supersede:{new_id}:{old}")),
        JudgeAction::DuplicateOf { survivor, p } => {
            let mut ids = [new_id.to_string(), survivor.clone()];
            ids.sort();
            (ReviewType::Merge, serde_json::json!({"survivor": survivor, "others": [new_id], "p": p, "provider": provider}), *p, format!("merge:{}", ids.join(":")))
        }
        // `decide` already asked about the contradiction.
        JudgeAction::Contradicts { .. } => return None,
    };
    Some(NewReview { kind, payload, confidence: Some(p), dedupe_key: Some(key) })
}
```

(`NewReview` needs `#[derive(Debug, Clone, PartialEq)]` — add `PartialEq` in `reviews.rs`.)

`store/judge.rs` additions:

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct PendingJudgement {
    pub seq: i64,
    pub id: String,
    /// `content_hash` when read: the judgement applies only if the text is unchanged.
    pub stamp: String,
    pub project_id: String,
    pub project_slug: String,
    pub kind_explicit: bool,
    pub project_defaulted: bool,
    pub attempts: u32,
}

// `judge_state <> 'done'` repeats the partial index's term so SQLite uses `memories_judge`.
const PENDING_SQL: &str = "SELECT m.seq, m.id, m.content_hash, m.project_id, p.slug, m.kind_explicit, m.project_defaulted, m.judge_attempts
     FROM memories m JOIN projects p ON p.id = m.project_id
     WHERE m.judge_state <> 'done' AND m.judge_state = 'pending' AND m.embed_state IN ('done', 'failed') AND m.status <> 'trashed'
     ORDER BY m.seq LIMIT ?1";

/// Memories whose embedding step is over and that wait for the judge, oldest first.
pub fn pending(conn: &Connection, limit: usize) -> Result<Vec<PendingJudgement>> {
    let mut stmt = conn.prepare_cached(PENDING_SQL)?;
    Ok(stmt
        .query_map([limit as i64], |r| Ok(PendingJudgement {
            seq: r.get(0)?, id: r.get(1)?, stamp: r.get(2)?, project_id: r.get(3)?, project_slug: r.get(4)?,
            kind_explicit: r.get::<_, i64>(5)? != 0, project_defaulted: r.get::<_, i64>(6)? != 0, attempts: r.get::<_, i64>(7)? as u32,
        }))?
        .collect::<std::result::Result<_, _>>()?)
}

pub fn judge_backlog(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM memories WHERE judge_state <> 'done' AND judge_state = 'pending' AND status <> 'trashed'",
        [], |r| r.get(0))?;
    Ok(n as u64)
}

/// `retry` → `pending` for another try; after `max_attempts` they stop (`done`).
/// Returns (requeued, given up).
pub fn requeue_retries(conn: &Connection, max_attempts: u32) -> Result<(usize, usize)> {
    let given_up = conn.execute(
        "UPDATE memories SET judge_state = 'done' WHERE judge_state <> 'done' AND judge_state = 'retry' AND judge_attempts >= ?1",
        [max_attempts])?;
    let requeued = conn.execute(
        "UPDATE memories SET judge_state = 'pending' WHERE judge_state <> 'done' AND judge_state = 'retry' AND judge_attempts < ?1",
        [max_attempts])?;
    Ok((requeued, given_up))
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-core judge::pipeline store::` — Expected: pass.

- [ ] **Step 5: `ApplyJudgement` in the writer; activation moves here**

```rust
#[derive(Debug, Clone)]
pub struct Judgement {
    pub seq: i64,
    pub id: String,
    pub stamp: String,
    pub provider: String,
    pub actions: Vec<JudgeAction>,
    pub reviews: Vec<NewReview>,
    /// Set the kind (the writer gave none and the judge is sure).
    pub kind: Option<Kind>,
    /// Move to this project slug (routing, auto-applied).
    pub project_to: Option<String>,
    /// The judge failed or ran out of budget: the heuristic decided; try again later.
    pub retry: bool,
    /// `(id, content_hash)` of every existing memory the judge saw. An action on one that has
    /// changed since becomes a review item instead (nothing is applied on stale information).
    pub seen: Vec<(String, String)>,
}
```

In `apply`:

```rust
        WriteOp::ApplyJudgement(j) => {
            let row: Option<(String, String)> = conn
                .query_row("SELECT content_hash, status FROM memories WHERE seq = ?1", [j.seq], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            let Some((hash, status)) = row else { return Ok(Applied::Done(WriteOutcome::Done)) };
            if hash != j.stamp {
                // Edited while being judged: judge_state stays pending and the new text is judged.
                return Ok(Applied::Done(WriteOutcome::Done));
            }
            let actor = Actor::Judge(j.provider.clone());
            let group = alice_core::ids::new_id();
            let mut trashed = status == Status::Trashed.as_str();
            if !trashed {
                if let Some(slug) = &j.project_to
                    && let Some(target) = projects::resolve_slug(conn, slug)?
                {
                    let before = snap(conn, &j.id)?;
                    let h = st.hlc.tick();
                    match memories::move_to_project(conn, &j.id, &target.id, &h)? {
                        memories::MoveOutcome::Moved => record(conn, st, EventInput { actor: &actor, action: "memory.move", target_type: "memory", target_id: &j.id, before, after: snap(conn, &j.id)?, group: Some(&group) })?,
                        memories::MoveOutcome::DuplicateOf(other) => { merge(conn, st, &other, std::slice::from_ref(&j.id), &actor, Some(&group))?; trashed = true; }
                    }
                }
            }
            if !trashed && let Some(kind) = j.kind {
                let before = snap(conn, &j.id)?;
                let h = st.hlc.tick();
                if memories::apply_update(conn, &j.id, &MemoryUpdate { action: UpdateAction::Edit, kind: Some(kind), ..Default::default() }, h).is_ok() {
                    record(conn, st, EventInput { actor: &actor, action: "memory.edit", target_type: "memory", target_id: &j.id, before, after: snap(conn, &j.id)?, group: Some(&group) })?;
                }
            }
            let unchanged = |conn: &Connection, id: &str| -> Result<bool> {
                let want = j.seen.iter().find(|(i, _)| i == id).map(|(_, h)| h.as_str());
                let now: Option<(String, String)> = conn
                    .query_row("SELECT content_hash, status FROM memories WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()?;
                Ok(matches!((want, now), (Some(w), Some((h, st))) if w == h && st != Status::Trashed.as_str()))
            };
            let mut reviews = j.reviews.clone();
            for a in &j.actions {
                if trashed {
                    break;
                }
                let target = match a {
                    JudgeAction::DuplicateOf { survivor, .. } => survivor,
                    JudgeAction::Supersedes { old, .. } => old,
                    JudgeAction::Contradicts { other, .. } => other,
                };
                if !unchanged(conn, target)? {
                    // Edited or forgotten since it was judged: ask instead of acting.
                    if let Some(r) = alice_core::judge::pipeline::as_review(&j.id, a, &j.provider) {
                        reviews.push(r);
                    }
                    continue;
                }
                match a {
                    JudgeAction::DuplicateOf { survivor, .. } => {
                        merge(conn, st, survivor, std::slice::from_ref(&j.id), &actor, Some(&group))?;
                        trashed = true;
                    }
                    JudgeAction::Supersedes { old, .. } => supersede(conn, st, old, &j.id, &actor, Some(&group))?,
                    JudgeAction::Contradicts { other, .. } => {
                        let before = snap(conn, &j.id)?;
                        memories::link(conn, &j.id, other, "contradicts", &actor.as_string())?;
                        record(conn, st, EventInput { actor: &actor, action: "memory.link", target_type: "memory", target_id: &j.id, before, after: snap(conn, &j.id)?, group: Some(&group) })?;
                    }
                }
            }
            for r in &reviews {
                if let Some(id) = reviews::create(conn, r)? {
                    st.stream.push(StreamItem::Review { id, review_type: r.kind.as_str().into() });
                }
            }
            if !trashed {
                activate(conn, j.seq, st.matcher(conn)?)?; // pending → active, mention links
            }
            conn.execute(
                "UPDATE memories SET judge_state = ?1, judge_attempts = judge_attempts + 1 WHERE seq = ?2",
                params![if j.retry { "retry" } else { "done" }, j.seq],
            )?;
            WriteOutcome::Done
        }
```

(`merge` and `supersede` are Task 11's writer helpers, which already take `group: Option<&str>`: pass `Some(&group)` here so one judgement is one undoable group.) In `StoreEmbeddings` and `EmbedFailed`, delete the `activate(..)` calls (activation now happens after judging). `worker.rs`: `WorkerConfig.judge_wake: Option<Sender<()>>`; after a successful `StoreEmbeddings` or `EmbedFailed`, `try_send(())` on it. Update the M1 worker tests: `pending_memories_and_chunks_are_embedded_activated_and_indexed` becomes `…_embedded_and_indexed` and asserts `status = 'pending'`, `judge_state = 'pending'`, `embed_state = 'done'` (no judge runs in that rig); `a_failing_embedder_is_retried_with_backoff_then_given_up` asserts `embed_state = 'failed'` and `status = 'pending'` (still live: pending memories are searched).

The three M1 tests in `writer.rs` that assumed activation on embedding change as follows (each still tests what its name says, now through `ApplyJudgement`):

- `stored_embeddings_apply_only_to_the_text_they_were_made_from`: after the `StoreEmbeddings` that matches the current text, assert `status = 'pending'`, `embed_state = 'done'`, `judge_state = 'pending'` (it used to assert `active`); then submit `WriteOp::ApplyJudgement(Judgement { seq, id, stamp: <the current content_hash>, provider: "heuristic".into(), actions: vec![], reviews: vec![], kind: None, project_to: None, retry: false, seen: vec![] })` and assert `status = 'active'`, `judge_state = 'done'`. Keep its stale-text half unchanged (embeddings for an old text are still dropped).
- `giving_up_on_an_embedding_activates_the_memory_but_not_after_an_edit` → rename `giving_up_on_an_embedding_leaves_the_memory_for_the_judge_but_not_after_an_edit`: after `EmbedFailed { retry: false }` assert `embed_state = 'failed'`, `status = 'pending'`; an `ApplyJudgement` with the current stamp then activates it; the edit half stays (a give-up for an old text changes nothing).
- `mentions_use_entities_added_after_the_matcher_was_loaded`: its store helper also submits that empty `ApplyJudgement` (heuristic, no actions) for the new memory before counting mentions, so `mentions == 1` is asserted on an active memory as before. (Its `WriteOp::AssertFact` construction is already Task 4's struct form `{ fact, actor }`.)

Writer test (Review Focus 4: an action is never applied on stale information):

```rust
    #[test]
    fn a_judgement_on_an_old_memory_edited_meanwhile_becomes_a_review_item() {
        let f = fixture();
        let (old, _) = new_id(f.h.submit_blocking(remember("Staging Postgres listens on port 5432")).unwrap());
        let (new, new_seq) = new_id(f.h.submit_blocking(remember("Staging Postgres now listens on 5433")).unwrap());
        let old_hash: String = f.read("SELECT content_hash FROM memories WHERE id = ?1", [&old]);
        let new_hash: String = f.read("SELECT content_hash FROM memories WHERE id = ?1", [&new]);
        // The old memory is edited after the judge read it.
        f.h.submit_blocking(WriteOp::Update { id: old.clone(), update: MemoryUpdate { body: Some("Staging Postgres listens on 6543".into()), ..Default::default() }, actor: Actor::User }).unwrap();
        f.h.submit_blocking(WriteOp::ApplyJudgement(Judgement {
            seq: new_seq, id: new.clone(), stamp: new_hash, provider: "jev".into(),
            actions: vec![JudgeAction::Supersedes { old: old.clone(), p: 0.95 }],
            reviews: vec![], kind: None, project_to: None, retry: false, seen: vec![(old.clone(), old_hash)],
        })).unwrap();
        let status: String = f.read("SELECT status FROM memories WHERE id = ?1", [&old]);
        assert_ne!(status, "superseded", "not applied on stale information");
        let n: i64 = f.read("SELECT count(*) FROM review_items WHERE type = 'supersede' AND status = 'open'", []);
        assert_eq!(n, 1);
    }
```

- [ ] **Step 6: The judge worker**

`crates/alice-daemon/src/judge_worker.rs`:

```rust
//! The judge stage of the write path (spec §5.1 step 5): for each memory whose embedding step is
//! over, compare it with its nearest memories in the project, set its kind or project when asked,
//! decide with the thresholds, and hand the judgement to the writer, which applies it and
//! activates the memory. Network calls (Jev) happen here, never on the writer.

use crate::config::SettingsHandle;
use crate::redaction::Redactors;
use crate::service::IndexSlot;
use crate::writer::{Judgement, WriteOp, WriterHandle};
use alice_core::embed::cosine;
use alice_core::error::{AliceError, Result};
use alice_core::judge::heuristic::HeuristicJudge;
use alice_core::judge::pipeline::{Thresholds, decide};
use alice_core::judge::text::title_similarity;
use alice_core::judge::{Judge, JudgeError, JudgeMemory, ProjectOption, Relation};
use alice_core::model::Kind;
use alice_core::store::ReaderPool;
use alice_core::store::embeddings::{self, VecKey};
use alice_core::store::judge::{PendingJudgement, pending};
use alice_core::store::memories;
use alice_core::store::reviews::{NewReview, ReviewType};
use alice_core::vector::KeyFilter;
use crossbeam_channel::Receiver;
use parking_lot::RwLock;
use rusqlite::Connection;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// The service's current judge and the warning that came with it.
pub type JudgeSlot = RwLock<(Arc<dyn Judge>, Option<String>)>;

const NEIGHBOURS: usize = 5;
const TITLE_CANDIDATES: usize = 3;
const BATCH: usize = 8;

pub struct JudgeWorkerConfig {
    pub readers: Arc<ReaderPool>,
    pub writer: WriterHandle,
    pub index: Arc<IndexSlot>,
    pub judge: Arc<JudgeSlot>,
    pub settings: Arc<SettingsHandle>,
    pub redactors: Arc<Redactors>,
    /// The embedding model whose stored vectors the judge reads (follows a model switch).
    pub model: Arc<dyn Fn() -> String + Send + Sync>,
    pub wake: Receiver<()>,
    pub stop: Arc<AtomicBool>,
}

pub fn spawn_judge_worker(cfg: JudgeWorkerConfig) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("alice-judge".into())
        .spawn(move || run(cfg))
        .expect("spawn judge thread")
}

fn run(cfg: JudgeWorkerConfig) {
    while !cfg.stop.load(Ordering::Relaxed) {
        if cfg.writer.read_only.reason().is_some() {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        let Some(_) = cfg.index.read().clone() else {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        };
        let items = match cfg.readers.with(|c| pending(c, BATCH)) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, "reading the judge queue failed");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        if items.is_empty() {
            let _ = cfg.wake.recv_timeout(Duration::from_millis(500));
            continue;
        }
        for item in items {
            if cfg.stop.load(Ordering::Relaxed) {
                break;
            }
            match judge_one(&cfg, &item) {
                Ok(j) => {
                    if let Err(e) = cfg.writer.submit_blocking(WriteOp::ApplyJudgement(j)) {
                        tracing::warn!(error = %e, id = %item.id, "applying a judgement failed");
                    }
                }
                Err(e) => tracing::warn!(error = %e, id = %item.id, "judging failed; it will be retried"),
            }
        }
    }
}

fn load(c: &Connection, seq: i64, slug: &str, similarity: Option<f32>) -> Result<Option<JudgeMemory>> {
    Ok(memories::get_by_seqs(c, &[seq])?.pop().map(|(_, m)| JudgeMemory {
        id: m.id, project: slug.to_string(), kind: m.kind, title: m.title, body: m.body,
        importance: m.importance, created_at: m.created_at, similarity,
    }))
}

/// Up to 5 nearest live memories of the same project by vector, plus up to 3 more of the same
/// kind whose titles are nearly the same (the heuristic's "updates" signal).
fn candidates(cfg: &JudgeWorkerConfig, c: &Connection, item: &PendingJudgement, me: &JudgeMemory) -> Result<Vec<JudgeMemory>> {
    let model = (cfg.model)();
    let my_vec = embeddings::get_many(c, &model, &[VecKey::Memory(item.seq)])?.pop().map(|(_, v)| v);
    let mut out: Vec<JudgeMemory> = Vec::new();
    if let (Some(v), Some(index)) = (&my_vec, cfg.index.read().clone()) {
        let hits = index.search(v, 50, KeyFilter::Memories)?;
        let seqs: Vec<i64> = hits.iter().filter_map(|(k, _)| match k { VecKey::Memory(s) if *s != item.seq => Some(*s), _ => None }).collect();
        let rows = memories::get_by_seqs(c, &seqs)?;
        for (seq, m) in rows {
            if out.len() == NEIGHBOURS {
                break;
            }
            let live = matches!(m.status, alice_core::model::Status::Active | alice_core::model::Status::Pending);
            if m.project == item.project_slug && live {
                let sim = hits.iter().find(|(k, _)| *k == VecKey::Memory(seq)).map(|(_, s)| *s);
                out.push(JudgeMemory { id: m.id, project: m.project, kind: m.kind, title: m.title, body: m.body,
                    importance: m.importance, created_at: m.created_at, similarity: sim });
            }
        }
    }
    let mut stmt = c.prepare_cached(
        "SELECT seq FROM memories WHERE project_id = ?1 AND kind = ?2 AND status IN ('active','pending') AND seq <> ?3
         ORDER BY updated_at DESC LIMIT 200",
    )?;
    let same_kind: Vec<i64> = stmt.query_map(rusqlite::params![item.project_id, me.kind.as_str(), item.seq], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let mut added = 0;
    for (seq, m) in memories::get_by_seqs(c, &same_kind)? {
        if added == TITLE_CANDIDATES {
            break;
        }
        if out.iter().any(|o| o.id == m.id) || title_similarity(&me.title, &m.title) < 0.9 {
            continue;
        }
        let sim = match &my_vec {
            Some(v) => embeddings::get_many(c, &model, &[VecKey::Memory(seq)])?.pop().map(|(_, w)| cosine(v, &w)),
            None => None,
        };
        out.push(JudgeMemory { id: m.id, project: m.project, kind: m.kind, title: m.title, body: m.body,
            importance: m.importance, created_at: m.created_at, similarity: sim });
        added += 1;
    }
    Ok(out)
}

/// Judge one memory: the configured judge unless the project is cloud-denied; the heuristic on
/// any failure (and then `retry`).
pub fn judge_one(cfg: &JudgeWorkerConfig, item: &PendingJudgement) -> Result<Judgement> {
    let settings = cfg.settings.get();
    let (judge, _) = cfg.judge.read().clone();
    let heuristic = HeuristicJudge::default();
    let (me, existing, root) = cfg.readers.with(|c| {
        let me = load(c, item.seq, &item.project_slug, None)?.ok_or_else(|| AliceError::NotFound(item.id.clone()))?;
        let existing = candidates(cfg, c, item, &me)?;
        let root = alice_core::store::projects::root_of(c, &item.project_id)?;
        Ok((me, existing, root))
    })?;
    let privacy = cfg.redactors.privacy_for(&settings.privacy, root.as_deref().map(std::path::Path::new));
    let project_denied = alice_core::settings::load_project_denied(root.as_deref()) || privacy.cloud_denied_projects.contains(&item.project_slug);
    let use_model = judge.provider() != "heuristic" && !project_denied;
    let mut retry = false;
    let mut with_fallback = |f: &dyn Fn(&dyn Judge) -> alice_core::judge::JudgeResult<Vec<Relation>>| -> (Vec<Relation>, String) {
        if use_model {
            match f(judge.as_ref()) {
                Ok(r) => return (r, judge.provider().to_string()),
                Err(JudgeError::Denied) => {}
                Err(e) => {
                    tracing::info!(error = %e, id = %item.id, "the judge failed; the heuristic decides for now");
                    retry = true;
                }
            }
        }
        (f(&heuristic).unwrap_or_default(), "heuristic".into())
    };
    let (relations, provider) = if existing.is_empty() {
        (Vec::new(), if use_model { judge.provider().to_string() } else { "heuristic".into() })
    } else {
        with_fallback(&|j: &dyn Judge| j.compare(&me, &existing))
    };
    let t = Thresholds { auto: settings.judge.auto_apply_threshold, review: settings.judge.review_threshold };
    let mut decided = decide(&me, &relations, &t, &provider);
    // Kind: only a model judge may change it (the heuristic's rules are never sure enough).
    // A failed classification marks the memory for a later retry, like a failed comparison
    // (spec §5.2, Plan decision 11).
    let kind = if item.kind_explicit || !use_model {
        None
    } else {
        match judge.classify_kind(&me) {
            Ok(d) => d
                .best()
                .filter(|(_, p)| *p >= t.auto)
                .and_then(|(k, _)| Kind::parse(&k))
                .filter(|k| *k != me.kind),
            Err(JudgeError::Denied) => None,
            Err(e) => {
                tracing::info!(error = %e, id = %item.id, "kind classification failed; asked again later");
                retry = true;
                None
            }
        }
    };
    // Project routing for memories that fell back to global.
    let mut project_to = None;
    if item.project_defaulted {
        let projects: Vec<ProjectOption> = cfg.readers.with(|c| {
            let mut stmt = c.prepare("SELECT slug, name FROM projects WHERE slug <> 'global' ORDER BY created_at DESC LIMIT 50")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut out = Vec::new();
            for row in rows {
                let (slug, name) = row?;
                let hint: Vec<String> = c.prepare_cached(
                    "SELECT m.title FROM memories m JOIN projects p ON p.id = m.project_id WHERE p.slug = ?1 AND m.status = 'active' ORDER BY m.updated_at DESC LIMIT 3")?
                    .query_map([&slug], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
                out.push(ProjectOption { slug, name, hint: hint.join("; ") });
            }
            Ok(out)
        })?;
        if !projects.is_empty() {
            let routed = if use_model { judge.route_project(&me, &projects).ok() } else { heuristic.route_project(&me, &projects).ok() };
            if let Some((slug, p)) = routed.and_then(|d| d.best()).filter(|(s, _)| s != "global") {
                if p >= t.auto && use_model {
                    project_to = Some(slug);
                } else if p >= t.review {
                    decided.reviews.push(NewReview {
                        kind: ReviewType::Project,
                        payload: serde_json::json!({"memory_id": me.id, "from": "global", "to": slug, "p": p, "provider": provider}),
                        confidence: Some(p),
                        dedupe_key: Some(format!("project:{}", me.id)),
                    });
                }
            }
        }
    }
    let seen = cfg.readers.with(|c| {
        let mut out = Vec::new();
        for e in &existing {
            if let Some(h) = c.query_row("SELECT content_hash FROM memories WHERE id = ?1", [&e.id], |r| r.get::<_, String>(0)).optional()? {
                out.push((e.id.clone(), h));
            }
        }
        Ok(out)
    })?;
    Ok(Judgement { seq: item.seq, id: item.id.clone(), stamp: item.stamp.clone(), provider, actions: decided.actions,
        reviews: decided.reviews, kind, project_to, retry, seen })
}
```

Add to `alice_core::settings`:

```rust
/// Whether `<root>/.alice.toml` marks the project cloud-denied (false without a root or file).
pub fn load_project_denied(root: Option<&str>) -> bool {
    root.and_then(|r| load_project(Path::new(r)).ok()).is_some_and(|p| p.privacy.cloud_denied)
}
```

`service.rs`: hold `judge: Arc<JudgeSlot>` (Task 9's field becomes this type), `redactors: Arc<Redactors>`; in `start`, create `(judge_wake_tx, judge_wake_rx) = crossbeam_channel::bounded(1)`, pass `judge_wake: Some(judge_wake_tx)` to the embedding worker, and spawn `spawn_judge_worker(JudgeWorkerConfig { …, model: Arc::new(move || model.clone()), wake: judge_wake_rx, stop: stop.clone() })`; join it in `shutdown` before the writer. `health()` adds `judge_backlog: store::judge::judge_backlog(c)?`, `judge_provider`, `judge_warning`. Replace the service tests' `wait_embedded` with:

```rust
    /// Until the embedding and judge stages have finished every memory.
    async fn wait_settled(s: &Service) {
        for _ in 0..400 {
            let h = s.health().await.unwrap();
            if h.embed_backlog == 0 && h.judge_backlog == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("background work did not settle");
    }
```

(`remember_recall_get_update_round_trip`'s `status == Active` assertion then holds again.)

- [ ] **Step 7: Scripted Jev for service tests**

In `judge_http.rs`:

```rust
/// Test transport: answers every question of a request from fixed choices — relation questions
/// (`fwd_*`, `rev_*`) with `relation`, the kind question with `kind`, the project question with
/// `project` — or fails with `status` when set.
#[cfg(test)]
pub(crate) struct ScriptedJev {
    pub relation: (&'static str, f64),
    pub kind: Option<(&'static str, f64)>,
    pub project: Option<(&'static str, f64)>,
    pub status: Option<u16>,
    pub calls: parking_lot::Mutex<usize>,
}

#[cfg(test)]
impl JevTransport for ScriptedJev {
    fn post(&self, _e: &str, _k: &str, body: &[u8], _t: Duration) -> Result<HttpReply, TransportError> {
        *self.calls.lock() += 1;
        if let Some(status) = self.status {
            return Ok(HttpReply { status, body: b"{}".to_vec(), retry_after: Some(Duration::ZERO) });
        }
        let req: serde_json::Value = serde_json::from_slice(body).unwrap();
        let mut answers = serde_json::Map::new();
        for (qid, q) in req["questions"].as_object().unwrap() {
            let options: Vec<String> = q["criteria"].as_object().unwrap().keys().cloned().collect();
            let (choice, p) = if qid.starts_with("fwd_") {
                self.relation
            } else if qid.starts_with("rev_") {
                (if self.relation.0 == "updates" { "outdated_by" } else { self.relation.0 }, self.relation.1)
            } else if options.iter().any(|o| o == "bugfix") {
                self.kind.unwrap_or(("note", 1.0))
            } else {
                self.project.unwrap_or(("global", 1.0))
            };
            let rest = (1.0 - p) / (options.len() - 1) as f64;
            let probs: serde_json::Map<String, serde_json::Value> = options.iter()
                .map(|o| (o.clone(), serde_json::json!(if o == choice { p } else { rest }))).collect();
            answers.insert(qid.clone(), serde_json::json!({"type": "choice", "choice": choice, "confidence": p, "probabilities": probs}));
        }
        let resp = serde_json::json!({"model": "jev-1.13.0", "answers": answers, "usage": {"input_tokens": 500, "output_tokens": 2}});
        Ok(HttpReply { status: 200, body: serde_json::to_vec(&resp).unwrap(), retry_after: None })
    }
}
```

- [ ] **Step 8: Service tests for the pipeline**

```rust
    async fn svc_with_jev(t: Arc<crate::judge_http::ScriptedJev>) -> (tempfile::TempDir, Arc<Service>) {
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions { jev_transport: Some(t), ..Default::default() },
        ).unwrap();
        let mut m = serde_json::Map::new();
        m.insert("judge.provider".into(), serde_json::json!("jev"));
        s.patch_settings(m).unwrap();
        s.set_secret(SecretName::JevApiKey, "test-key-0000".into()).unwrap();
        (d, s)
    }

    fn scripted(rel: (&'static str, f64)) -> Arc<crate::judge_http::ScriptedJev> {
        Arc::new(crate::judge_http::ScriptedJev { relation: rel, kind: None, project: None, status: None, calls: Default::default() })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_heuristic_trashes_a_near_identical_memory_and_get_returns_the_survivor() {
        let (d, s) = svc().await;
        let cwd = repo(&d, "proj");
        let a = s.remember(RememberRequest { content: "Use WAL mode so readers never block the writer".into(), kind: Some(Kind::Decision), cwd: cwd.clone(), ..Default::default() }).await.unwrap().id.unwrap();
        wait_settled(&s).await;
        // Same text under another kind: a new row (the content hash includes the kind) with an
        // identical vector, so cosine 1.0 ≥ 0.97.
        let b = s.remember(RememberRequest { content: "Use WAL mode so readers never block the writer".into(), kind: Some(Kind::Note), cwd: cwd.clone(), ..Default::default() }).await.unwrap().id.unwrap();
        wait_settled(&s).await;
        let got = s.get(GetRequest { ids: vec![b.clone()], include_provenance: false }).await.unwrap();
        assert_eq!(got.memories[0].memory.id, a, "the survivor answers for the duplicate");
        assert_eq!(got.redirects[0].from, b);
        let ev = s.events(EventsRequest { target_id: Some(b.clone()), ..Default::default() }).await.unwrap();
        assert!(ev.iter().any(|e| e.action == "memory.merge" && e.actor == "judge:heuristic"), "{ev:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn jev_updates_supersede_contradictions_link_and_ask() {
        for (rel, expect_status, expect_review) in [(("updates", 0.95), "superseded", None), (("contradicts", 0.9), "active", Some("contradiction"))] {
            let (d, s) = svc_with_jev(scripted(rel)).await;
            let cwd = repo(&d, "proj");
            let old = s.remember(RememberRequest { content: "Staging Postgres listens on port 5432".into(), kind: Some(Kind::Fact), cwd: cwd.clone(), ..Default::default() }).await.unwrap().id.unwrap();
            wait_settled(&s).await;
            s.remember(RememberRequest { content: "Staging Postgres listens on port 5433 now".into(), kind: Some(Kind::Fact), cwd: cwd.clone(), ..Default::default() }).await.unwrap();
            wait_settled(&s).await;
            let got = s.get(GetRequest { ids: vec![old.clone()], include_provenance: false }).await.unwrap();
            assert_eq!(got.memories[0].memory.status.as_str(), expect_status, "{rel:?}");
            let open = s.reviews(ReviewQuery::default()).await.unwrap();
            assert_eq!(open.first().map(|r| r.review_type.as_str()), expect_review, "{rel:?}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_jev_falls_back_to_the_heuristic_and_retries_later_without_failing_the_write() {
        let t = Arc::new(crate::judge_http::ScriptedJev { relation: ("unrelated", 1.0), kind: None, project: None, status: Some(503), calls: Default::default() });
        let (d, s) = svc_with_jev(t.clone()).await;
        let state = |id: String| {
            let s = s.clone();
            async move {
                s.read(move |c| Ok(c.query_row("SELECT judge_state, status FROM memories WHERE id = ?1", [&id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?)).await.unwrap()
            }
        };
        // A: an explicit kind and no neighbour — nothing to ask, so no request at all.
        let a = s.remember(RememberRequest { content: "anything at all".into(), kind: Some(Kind::Note), cwd: repo(&d, "proj"), ..Default::default() }).await.unwrap();
        assert_eq!(a.status, RememberStatus::New, "a failing judge never fails the write");
        wait_settled(&s).await;
        assert_eq!(state(a.id.unwrap()).await, ("done".to_string(), "active".to_string()));
        assert_eq!(*t.calls.lock(), 0);
        // B: an explicit kind and a neighbour — the comparison's 503s exhaust the retries.
        let cwd = repo(&d, "proj3");
        s.remember(RememberRequest { content: "deploy notes for staging".into(), kind: Some(Kind::Note), cwd: cwd.clone(), ..Default::default() }).await.unwrap();
        wait_settled(&s).await;
        assert_eq!(*t.calls.lock(), 0, "the first memory of proj3 has no neighbour");
        let b = s.remember(RememberRequest { content: "deploy notes for staging servers".into(), kind: Some(Kind::Note), cwd, ..Default::default() }).await.unwrap().id.unwrap();
        wait_settled(&s).await;
        assert_eq!(*t.calls.lock(), 4, "one try and three retries for the comparison");
        assert_eq!(state(b).await, ("retry".to_string(), "active".to_string()), "the heuristic decides meanwhile");
        // C: no kind and no neighbour — the kind question fails; that is retried later too.
        let c = s.remember(RememberRequest { content: "something else entirely".into(), cwd: repo(&d, "proj2"), ..Default::default() }).await.unwrap().id.unwrap();
        wait_settled(&s).await;
        assert_eq!(*t.calls.lock(), 8, "one try and three retries for the kind question");
        assert_eq!(state(c).await, ("retry".to_string(), "active".to_string()), "a failed kind classification is asked again (spec §5.2)");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kind_is_set_only_when_the_writer_gave_none_and_cloud_denied_projects_stay_local() {
        let t = Arc::new(crate::judge_http::ScriptedJev { relation: ("unrelated", 1.0), kind: Some(("bugfix", 0.95)), project: None, status: None, calls: Default::default() });
        let (d, s) = svc_with_jev(t.clone()).await;
        let cwd = repo(&d, "proj");
        let a = s.remember(RememberRequest { content: "Fixed: the lease expired; mining now runs off the writer".into(), cwd: cwd.clone(), ..Default::default() }).await.unwrap().id.unwrap();
        let b = s.remember(RememberRequest { content: "We chose WAL".into(), kind: Some(Kind::Decision), cwd: cwd.clone(), ..Default::default() }).await.unwrap().id.unwrap();
        wait_settled(&s).await;
        let got = s.get(GetRequest { ids: vec![a, b], include_provenance: false }).await.unwrap();
        assert_eq!(got.memories[0].memory.kind, Kind::Bugfix);
        assert_eq!(got.memories[1].memory.kind, Kind::Decision, "an explicit kind is never changed");
        let calls = *t.calls.lock();
        std::fs::write(std::path::Path::new(cwd.as_deref().unwrap()).join(".alice.toml"), "[privacy]\ncloud_denied = true\n").unwrap();
        s.remember(RememberRequest { content: "secret project note".into(), cwd, ..Default::default() }).await.unwrap();
        wait_settled(&s).await;
        assert_eq!(*t.calls.lock(), calls, "nothing from a cloud-denied project is sent");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_memory_with_no_project_is_routed_by_the_judge() {
        let t = Arc::new(crate::judge_http::ScriptedJev { relation: ("unrelated", 1.0), kind: None, project: Some(("proj", 0.95)), status: None, calls: Default::default() });
        let (d, s) = svc_with_jev(t).await;
        s.remember(RememberRequest { content: "proj uses WAL".into(), cwd: repo(&d, "proj"), ..Default::default() }).await.unwrap();
        wait_settled(&s).await;
        // Neither project nor cwd: global for now, and the judge may route it.
        let id = s.remember(RememberRequest { content: "the proj deploy runs on Fridays".into(), ..Default::default() }).await.unwrap().id.unwrap();
        wait_settled(&s).await;
        let got = s.get(GetRequest { ids: vec![id.clone()], include_provenance: false }).await.unwrap();
        assert_eq!(got.memories[0].memory.project, "proj");
        let ev = s.events(EventsRequest { target_id: Some(id), ..Default::default() }).await.unwrap();
        assert!(ev.iter().any(|e| e.action == "memory.move" && e.actor == "judge:jev"), "{ev:?}");
    }
```

(`Service::read` is private: make it `pub(crate)` so these tests can query `judge_state`.)

Run: `nice -n 15 cargo test -j 6 --workspace` — Expected: pass (M1 tests adapted as described); `pgrep -fl "alice daemon run"` empty.

- [ ] **Step 9: Commit**

```bash
nice -n 15 cargo fmt --all && nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
git add crates
git commit -m "feat: judge every new memory against its neighbours; duplicates, supersedes, contradictions, kinds and project routing (schema v3)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 13: Predicate normalisation, precise recall, fact time validation

Spec §5.2 table: "Predicate normalization — which existing predicate does this one mean? Map to canonical or create a new one"; §8.1 `assert_fact` returns "a predicate suggestion if a similar one exists"; §4.4 step 4 "Precise mode: the Judge scores the top 20 for relevance in one batched call and results are re-ordered by P(relevant)". The M1 ledger also deferred fact-time validation to M2 (years outside 1–9999 break text ordering; a leap second `:60` produced a zero-length interval).

**Files:**
- Modify: `crates/alice-daemon/src/service.rs` (`assert_fact` predicate step; precise re-rank in `recall`)
- Modify: `crates/alice-daemon/src/writer.rs` (`WriteOp::AssertFact { fact, actor, alias, review }`)
- Modify: `crates/alice-core/src/store/kg.rs` (`predicate_exists`, `predicate_names`; `stored_time` guards), `crates/alice-core/src/api.rs` (`RecallResponse.reranked`), `crates/alice-core/src/search/mod.rs` (`SearchOutput` exposes what precise mode needs)
- Modify: `crates/alice-daemon/src/judge_http.rs` (`ScriptedJev.predicate`), `crates/alice-daemon/src/mcp.rs` (recall tool description mentions `mode: precise`)
- Modify: `crates/alice/src/cmd.rs` (`alice recall --precise`, spec §8.3), `crates/alice/tests/cli.rs`
- Modify (cross-task): every `WriteOp::AssertFact { fact, actor }` construction since Task 4 gains `alias: None, review: None`

**Interfaces:**
- Consumes: Task 8 (`Judge::{match_predicate, grade_relevance}`, `text::predicate_similarity`, `RELEVANCE_*`), Task 9 (`Service::judge`), Task 11 (`kg::map_predicate`, review type `predicate`), Task 12 (`ScriptedJev`).
- Produces:
  - `alice_core::store::kg::{predicate_exists(conn, name) -> Result<bool>, predicate_names(conn, limit) -> Result<Vec<String>>}` (canonical names only)
  - `WriteOp::AssertFact { fact: AssertFact, actor: Actor, alias: Option<(String, String)>, review: Option<NewReview> }` — `alias = (new, canonical)` is applied with `map_predicate` before the fact; `review` is created with it.
  - `FactOutcome.suggestion` filled when a similar predicate exists but the match is not sure enough to map.
  - `RecallResponse.reranked: bool` (`#[serde(default)]`): true when precise mode re-ordered the results.
  - Constants in `service.rs`: `PRECISE_TOP: usize = 20`, `PREDICATE_WAIT: Duration = 3 s`.
  - CLI: `alice recall … --precise` (sets `RecallRequest.mode = RecallMode::Precise`).

- [ ] **Step 1: Read the files this task modifies, as they are now.**

- [ ] **Step 2: Fact time validation, test first**

In `kg.rs` tests:

```rust
    #[test]
    fn fact_times_outside_four_digit_years_are_refused_and_leap_seconds_are_clamped() {
        let (_d, c) = conn(); // the module's M1 helper
        // "10000-01-01T00:00:00Z" fails RFC 3339 parsing (five-digit year) and reaches the
        // "not a time" branch; the others parse and hit the year guard. Both messages name the fix.
        for bad in ["+10000-01-01", "10000-01-01T00:00:00Z", "-0001-06-01", "0000-01-01"] {
            let e = assert_fact(&c, &AssertFact { subject: "a".into(), predicate: "p".into(), object: "o".into(), valid_from: Some(bad.into()), ..Default::default() }, "h".into()).unwrap_err();
            assert!(e.to_string().contains("four-digit year"), "{bad}: {e}");
        }
        let out = assert_fact(&c, &AssertFact { subject: "a".into(), predicate: "p".into(), object: "o".into(),
            valid_from: Some("2026-06-30T23:59:60Z".into()), ..Default::default() }, "h".into()).unwrap();
        let from: String = c.query_row("SELECT valid_from FROM relations WHERE id = ?1", [out.relation_id.unwrap()], |r| r.get(0)).unwrap();
        assert_eq!(from, "2026-06-30T23:59:59.999Z");
    }
```

Implement in `stored_time`. The M1 "not a time" message becomes `"{field} \"{}\" is not a time; use RFC 3339 with a four-digit year such as 2026-10-13T09:44:23.391Z, or a date such as 2026-10-13"` (no M1 test matches its old text). After parsing to `utc`:

```rust
    use chrono::{Datelike, Timelike};
    if !(1..=9999).contains(&utc.year()) {
        return Err(AliceError::invalid(format!(
            "{field} \"{}\" has a year outside 1 to 9999; use a four-digit year such as 2026-10-13",
            truncate_chars(s, 40)
        )));
    }
    // A leap second (:60) is kept as the last millisecond of the minute, so intervals stay ordered.
    let utc = if utc.nanosecond() >= 1_000_000_000 {
        utc.with_nanosecond(999_000_000).unwrap_or(utc)
    } else {
        utc
    };
```

Run: `nice -n 15 cargo test -j 6 -p alice-core kg::` — Expected: pass.

- [ ] **Step 3: Write the failing predicate tests**

`ScriptedJev` gains `pub predicate: Option<(&'static str, f64)>` (answers questions whose options include `"none"`; default `("none", 1.0)`); detect predicate questions before the kind/project branches, and add `predicate: None` to Task 12's `ScriptedJev` constructions. Service tests:

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_predicate_maps_to_an_identical_stem_or_is_suggested() {
        let (_d, s) = svc().await;
        let f = |p: &str, o: &str| AssertFactRequest { subject: "alice".into(), predicate: p.into(), object: o.into(), ..Default::default() };
        s.assert_fact(f("uses", "SQLite")).await.unwrap();
        s.assert_fact(f("deployed_on", "Coolify")).await.unwrap();
        // Same stem: the heuristic maps it (an identical predicate is a duplicate).
        let out = s.assert_fact(f("use", "Rust")).await.unwrap();
        assert_eq!((out.predicate.as_str(), out.suggestion.as_deref()), ("uses", None));
        // Similar but not the same: stored as given, suggested, and asked in the review inbox.
        let out = s.assert_fact(f("deployed_to", "Railway")).await.unwrap();
        assert_eq!((out.predicate.as_str(), out.suggestion.as_deref()), ("deployed_to", Some("deployed_on")));
        let open = s.reviews(ReviewQuery::default()).await.unwrap();
        assert_eq!(open[0].review_type.as_str(), "predicate");
        assert_eq!(open[0].payload["canonical"], "deployed_on");
        // An unrelated new predicate: nothing to say.
        let out = s.assert_fact(f("owns", "a cat")).await.unwrap();
        assert_eq!(out.suggestion, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn jev_maps_a_paraphrased_predicate_when_sure() {
        let t = Arc::new(crate::judge_http::ScriptedJev { relation: ("unrelated", 1.0), kind: None, project: None, predicate: Some(("runs_on", 0.93)), status: None, calls: Default::default() });
        let (_d, s) = svc_with_jev(t).await;
        let f = |p: &str, o: &str| AssertFactRequest { subject: "api".into(), predicate: p.into(), object: o.into(), ..Default::default() };
        s.assert_fact(f("runs_on", "k8s")).await.unwrap();
        let out = s.assert_fact(f("is_hosted_on", "fly.io")).await.unwrap();
        assert_eq!(out.predicate, "runs_on");
        let facts = s.facts(FactsRequest { entity: "api".into(), ..Default::default() }).await.unwrap();
        assert!(facts.facts.iter().all(|f| f.predicate == "runs_on"));
    }
```

Run: `nice -n 15 cargo test -j 6 -p alice-daemon predicate` — Expected: FAIL (no mapping yet).

- [ ] **Step 4: Implement the predicate step**

`kg.rs`:

```rust
pub fn predicate_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(canonical_predicate(conn, &normalize_predicate(name))?.is_some())
}

/// Canonical predicate names, most used first.
pub fn predicate_names(conn: &Connection, limit: usize) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT p.name FROM predicates p LEFT JOIN relations r ON r.predicate = p.name
         WHERE p.canonical IS NULL GROUP BY p.name ORDER BY count(r.id) DESC, p.name LIMIT ?1",
    )?;
    Ok(stmt.query_map([limit as i64], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?)
}
```

`service.rs`:

```rust
/// How long `assert_fact` waits for the judge to match a new predicate before the heuristic decides.
const PREDICATE_WAIT: Duration = Duration::from_secs(3);

    pub async fn assert_fact(&self, req: AssertFactRequest) -> Result<FactOutcome> {
        let actor = Actor::from_author(&req.author);
        let name = alice_core::store::kg::normalize_predicate(&req.predicate);
        let (known, names) = {
            let n = name.clone();
            self.read(move |c| Ok((alice_core::store::kg::predicate_exists(c, &n)?, alice_core::store::kg::predicate_names(c, 500)?))).await?
        };
        let mut alias = None;
        let mut review = None;
        let mut suggestion = None;
        if !known && !name.is_empty() && req.action != FactAction::End && !names.is_empty() {
            let mut close: Vec<(String, f64)> = names
                .into_iter()
                .map(|p| { let s = alice_core::judge::text::predicate_similarity(&name, &p); (p, s) })
                .collect();
            // The ten most similar names go to the judge (a model judge can see through wording;
            // the heuristic only acts at Jaro-Winkler ≥ 0.9 anyway).
            close.sort_by(|a, b| b.1.total_cmp(&a.1));
            close.truncate(10);
            let candidates: Vec<String> = close.into_iter().map(|(p, _)| p).collect();
            if !candidates.is_empty() {
                let settings = self.settings();
                let judge = self.judge();
                let (n, c) = (name.clone(), candidates.clone());
                let asked = tokio::time::timeout(PREDICATE_WAIT, tokio::task::spawn_blocking(move || {
                    let d = judge.match_predicate(&n, &c);
                    (judge.provider().to_string(), d)
                })).await;
                let (provider, d) = match asked {
                    Ok(Ok((p, Ok(d)))) => (p, d),
                    _ => ("heuristic".to_string(), HeuristicJudge::default().match_predicate(&name, &candidates).unwrap_or_default()),
                };
                if let Some((canonical, p)) = d.best().filter(|(c, _)| c != "none") {
                    if p >= settings.judge.auto_apply_threshold {
                        alias = Some((name.clone(), canonical));
                    } else if p >= settings.judge.review_threshold {
                        suggestion = Some(canonical.clone());
                        review = Some(NewReview {
                            kind: ReviewType::Predicate,
                            payload: serde_json::json!({"predicate": name, "canonical": canonical, "p": p, "provider": provider}),
                            confidence: Some(p),
                            dedupe_key: Some(format!("predicate:{name}")),
                        });
                    }
                }
            }
        }
        match self.writer.submit(WriteOp::AssertFact { fact: req, actor, alias, review }).await? {
            WriteOutcome::Fact(mut f) => {
                f.suggestion = f.suggestion.or(suggestion);
                Ok(f)
            }
            other => Err(unexpected(other)),
        }
    }
```

(The heuristic's identical-stem answer is 0.95, so it maps; its "similar" answer is 0.7, so it suggests — the spec's "heuristic auto-applies only duplicates".) `writer.rs` `AssertFact`: if `alias` is `Some((new, canonical))`, call `kg::map_predicate(conn, &new, &canonical, &st.hlc.tick())` first and record a `predicate.map` event; then assert as before (its predicate now resolves to the canonical one); then create `review` if any (and push `StreamItem::Review`). **Cross-task edit:** the two new fields break every `WriteOp::AssertFact { fact, actor }` built since Task 4 — Task 4's writer test `a_fact_supersede_records_the_closed_and_the_new_relation_in_one_group`, Task 12's `mentions_use_entities_added_after_the_matcher_was_loaded` helper and `Service::assert_fact` — add `alias: None, review: None` to each (search `WriteOp::AssertFact {` across `crates/`).

Run: `nice -n 15 cargo test -j 6 -p alice-daemon predicate` — Expected: pass.

- [ ] **Step 5: Precise recall, test first**

```rust
    /// Grades results that contain `keyword` as answering the query, the rest as unrelated.
    struct KeywordGrader(&'static str);

    impl alice_core::judge::jev::JevTransport for KeywordGrader {
        fn post(&self, _e: &str, _k: &str, body: &[u8], _t: Duration) -> Result<alice_core::judge::jev::HttpReply, alice_core::judge::jev::TransportError> {
            let req: serde_json::Value = serde_json::from_slice(body).unwrap();
            let state = req["state"].as_str().unwrap();
            let mut answers = serde_json::Map::new();
            for (qid, _) in req["questions"].as_object().unwrap() {
                let k: usize = qid.trim_start_matches("r_").parse().unwrap_or(0);
                let block = state.split(&format!("Result {k} (")).nth(1).unwrap_or("").split("\nResult ").next().unwrap_or("");
                let (choice, other) = if block.contains(self.0) { ("answers", "unrelated") } else { ("unrelated", "answers") };
                answers.insert(qid.clone(), serde_json::json!({"choice": choice, "confidence": 0.9,
                    "probabilities": {choice: 0.9, other: 0.05, "partially": 0.05}}));
            }
            let resp = serde_json::json!({"answers": answers, "usage": {"input_tokens": 800, "output_tokens": 0}});
            Ok(alice_core::judge::jev::HttpReply { status: 200, body: serde_json::to_vec(&resp).unwrap(), retry_after: None })
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn precise_mode_reorders_by_the_judges_relevance_and_fast_mode_does_not() {
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions { jev_transport: Some(Arc::new(KeywordGrader("ZEBRA"))), ..Default::default() }).unwrap();
        let mut m = serde_json::Map::new();
        m.insert("judge.provider".into(), serde_json::json!("jev"));
        s.patch_settings(m).unwrap();
        s.set_secret(SecretName::JevApiKey, "test-key-0000".into()).unwrap();
        let cwd = repo(&d, "proj");
        for text in ["deploy notes: staging deploy runs make deploy", "deploy notes: the ZEBRA cluster deploy uses blue green", "deploy notes: deploy freeze on Fridays"] {
            s.remember(RememberRequest { content: text.into(), kind: Some(Kind::Howto), cwd: cwd.clone(), ..Default::default() }).await.unwrap();
        }
        wait_settled(&s).await;
        let q = |mode| RecallRequest { query: "deploy notes".into(), mode, cwd: cwd.clone(), format: OutputFormat::Json, ..Default::default() };
        let fast = s.recall(q(RecallMode::Fast)).await.unwrap();
        assert!(!fast.reranked);
        let precise = s.recall(q(RecallMode::Precise)).await.unwrap();
        assert!(precise.reranked);
        assert!(precise.results[0].title.contains("ZEBRA"), "{:?}", precise.results.iter().map(|h| &h.title).collect::<Vec<_>>());
        assert_eq!(precise.results.len(), fast.results.len());
    }
```

Run: `nice -n 15 cargo test -j 6 -p alice-daemon precise_mode` — Expected: FAIL (`reranked` missing).

- [ ] **Step 6: Implement precise mode**

`api.rs`: `#[serde(default)] pub reranked: bool` on `RecallResponse`. `service.rs` `recall`, after `search::run` returns `out` and before packing:

```rust
/// Results the judge grades in precise mode (spec §4.4 step 4).
const PRECISE_TOP: usize = 20;

        let mut reranked = false;
        if req_mode == RecallMode::Precise {
            let judge = self.judge();
            if judge.provider() != "heuristic" {
                let graded: Vec<(usize, JudgeMemory)> = out.hits.iter().take(PRECISE_TOP).enumerate()
                    .filter(|(_, h)| h.tier == Tier::Knowledge)
                    .map(|(i, h)| (i, JudgeMemory {
                        id: h.id.clone(), project: h.project.clone(), kind: h.kind.unwrap_or(Kind::Note),
                        title: h.title.clone(), body: h.body.clone().unwrap_or_else(|| h.snippet.clone()),
                        importance: 3, created_at: h.created_at.clone(), similarity: None,
                    }))
                    .collect();
                let query = query_text.clone();
                let ms: Vec<JudgeMemory> = graded.iter().map(|(_, m)| m.clone()).collect();
                let wait = Duration::from_millis(self.settings().judge.timeout_ms);
                let asked = tokio::time::timeout(wait, tokio::task::spawn_blocking(move || judge.grade_relevance(&query, &ms))).await;
                if let Ok(Ok(Ok(grades))) = asked {
                    // P(relevant) = P(answers) + ½·P(partially); results the judge did not grade
                    // (raw chunks, cloud-denied projects) keep their places.
                    let mut scored: Vec<(usize, f64)> = graded.iter().zip(&grades)
                        .filter(|(_, d)| !d.0.is_empty())
                        .map(|((i, _), d)| (*i, d.p(RELEVANCE_ANSWERS) + 0.5 * d.p(RELEVANCE_PARTIAL)))
                        .collect();
                    let slots: Vec<usize> = scored.iter().map(|(i, _)| *i).collect();
                    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                    let original = out.hits.clone();
                    for (slot, (from, _)) in slots.iter().zip(&scored) {
                        out.hits[*slot] = original[*from].clone();
                    }
                    reranked = !scored.is_empty();
                }
            }
        }
```

(`req_mode` and `query_text` are copies of `req.mode` and the query taken before `req` moves into the read closure; `memory_seqs` for access counting are unaffected by the order.) Set `reranked` on both the text and JSON responses (`pack_json` result: `r.reranked = reranked`). In `mcp.rs`, extend the `recall` tool description: "…Pass mode \"precise\" to have the judge re-rank the top results (slower; needs judge.provider = jev)."

CLI (spec §8.3 lists `alice recall … --precise`): in `cmd.rs` `RecallArgs` add

```rust
    /// Let the judge re-rank the top results (slower; uses Jev when it is configured).
    #[arg(long)]
    pub precise: bool,
```

and in `Command::Recall` set `mode: if a.precise { RecallMode::Precise } else { RecallMode::Fast },` in the `RecallRequest`. Text output: when `reranked` is true, end with the line `(re-ranked by the judge)`. Test in `crates/alice/tests/cli.rs`:

```rust
#[test]
fn recall_precise_reaches_the_daemon() {
    let env = Env::new();
    env.ok(&["remember", "precise flag check about lease timeouts"]);
    let fast = env.json(&["recall", "lease timeouts"]);
    assert_eq!(fast["reranked"], false, "fast mode never re-ranks");
    let precise = env.json(&["recall", "lease timeouts", "--precise"]);
    assert!(precise["reranked"].is_boolean(), "{precise}");
    assert_eq!(precise["results"].as_array().unwrap().len(), fast["results"].as_array().unwrap().len());
    let help = env.run(&["recall", "--help"]);
    assert!(String::from_utf8_lossy(&help.stdout).contains("--precise"));
}
```

Run: `nice -n 15 cargo test -j 6 --workspace` — Expected: pass.

- [ ] **Step 7: Commit**

```bash
nice -n 15 cargo fmt --all && nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
git add crates
git commit -m "feat: predicate normalisation through the judge, precise recall re-ranking, fact time validation

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 14: Maintenance scheduler, backups and rotation, retention, purges

Spec §5.6: a low-priority worker runs when the daemon has been idle for 60 s and yields as soon as requests arrive — daily backup (SQLite online backup API; keep 7 daily + 4 weekly), daily `PRAGMA quick_check` and vector-index count check (rebuild on mismatch; §4.1 "daily, M2"), weekly FTS optimise, daily raw retention (`forever`, an age, or a size), weekly similar-memory clusters → merge suggestions, daily trash purge (> 30 days), hourly judge retries, spool drain (already every 5 s in `daemon.rs`). The M1 ledger adds: prune pre-migration backups (they were written to `ALICE_HOME` and never removed). This task also adds `alice backup now|list` and `alice maintenance status|run <job>` so the jobs can be run and checked on demand.

**Files:**
- Modify: root `Cargo.toml` (`rusqlite` features `["bundled", "backup"]`)
- Modify: `crates/alice-core/src/store/migrations.rs` (pre-migration backups go to `<home>/backups/alice-pre-migration-v<from>-<stamp>.db`) and Task 4's migration test
- Create: `crates/alice-core/src/store/maintenance.rs`; modify `store/mod.rs`, `store/reviews.rs` (`was_rejected`)
- Create: `crates/alice-daemon/src/backup.rs`, `crates/alice-daemon/src/maintenance.rs`; modify `lib.rs`
- Modify: `crates/alice-daemon/src/writer.rs` (`SetMeta`, `PurgeMemories`, `DeleteChunks`, `RequeueJudge`, `FtsMerge`, `PruneEvents`, `CreateReview`), `service.rs` (activity tracking, scheduler thread, `backup_now`, `backups`, `maintenance_status`, `run_job`; `spawn_rebuild` crate-visible), `http.rs`, `client.rs`, `crates/alice/src/{main.rs,cmd.rs}`
- Modify: `crates/alice-core/src/api.rs` (`JobStatus`)
- Modify: `docs/benchmarks/m2.md` (job durations at 1M chunks)

**Interfaces:**
- Consumes: Task 4 (`record`, events), Task 5 (`MaintenanceSettings`, `BackupSettings`, `Retention`), Task 11 (`reviews::create`, review type `merge`), Task 12 (`store::judge::requeue_retries`), Task 1 (`VectorIndex::{remove, remove_chunks}`).
- Produces:
  - `alice_core::store::maintenance::{trash_due(conn, cutoff, limit) -> Result<Vec<i64>>, purge_memories(conn, seqs) -> Result<Vec<(String, String)>>, chunks_older_than(conn, cutoff, limit) -> Result<Vec<i64>>, raw_bytes(conn) -> Result<u64>, oldest_chunks(conn, limit) -> Result<Vec<(i64, u64)>>, delete_chunks(conn, seqs) -> Result<usize>, fts_merge(conn, table, pages) -> Result<bool>, prune_events(conn, before_ts, limit) -> Result<usize>, quick_check(conn) -> Result<Vec<String>>, get_meta(conn, key) -> Result<Option<String>>}`
  - `alice_core::store::reviews::was_rejected(conn, dedupe_key) -> Result<bool>`
  - `alice_daemon::backup::{BackupKind::{Scheduled, Manual, PreMigration, BeforeRestore}, BackupInfo { name, path, kind, created_at, bytes }, backup_dir(home, &BackupSettings) -> PathBuf, create(db: &Path, dir: &Path, kind, now) -> Result<BackupInfo>, list(dir) -> Result<Vec<BackupInfo>>, rotate(dir, now, daily: u32, weekly: u32) -> Result<Vec<PathBuf>>, adopt_legacy(home, dir) -> Result<usize>}`
  - `alice_daemon::maintenance::{Activity, ActivityGuard, Job, JobOutcome, JobReport, Scheduler, MaintenanceCtx}`; `Job::{Backup, QuickCheck, IndexCheck, FtsOptimize, RawRetention, TrashPurge, JudgeRetry, MergeSuggestions, EventsPrune}` with `name()`, `parse()`, `every()`; `Scheduler::{due(&self, now) -> Vec<Job>, run_job(&self, job, now, force: bool) -> JobReport, spawn(self: Arc<Self>, stop) -> JoinHandle<()>}`; `JobOutcome` and `JobReport` derive `Serialize` and `Deserialize` (the client decodes `POST /api/v1/maintenance/run`)
  - Meta keys: `maint.<job>.last_run`, `maint.<job>.last_ok`, `maint.<job>.last_error`, `maint.merge.cursor`.
  - `Service::{activity(&self) -> &Arc<Activity>, backup_now(&self) -> Result<BackupInfo>, backups(&self) -> Result<Vec<BackupInfo>>, maintenance_status(&self) -> Result<Vec<JobStatus>>, run_job(&self, job: Job) -> Result<JobReport>}`; `JobStatus { job, every_secs, last_run, last_ok, last_error }` in `alice_core::api`.
  - HTTP: `GET /api/v1/backups`, `POST /api/v1/backups`, `GET /api/v1/maintenance`, `POST /api/v1/maintenance/run` (`{"job": "trash_purge"}`). CLI: `alice backup [now|list]`, `alice maintenance [status|run <job>]`.

- [ ] **Step 1: Read the files this task modifies, as they are now.**

- [ ] **Step 2: Pre-migration backups move into `backups/`**

In `migrations.rs`, replace the backup destination with:

```rust
    if !applied.is_empty() {
        let from = applied.iter().max().copied().unwrap_or(0);
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let dir = db_path.parent().unwrap_or(Path::new(".")).join("backups");
        std::fs::create_dir_all(&dir)?;
        let dest = dir.join(format!("alice-pre-migration-v{from}-{stamp}.db"));
        super::backup_into(conn, &dest)?;
    }
```

(`chrono` is already an `alice-core` dependency.) Update Task 4's `a_v1_database_migrates_to_v2_after_a_backup` to look for the file in `<dir>/backups/` and assert its name starts with `alice-pre-migration-v1-`.

- [ ] **Step 3: Write the failing store tests**

`crates/alice-core/src/store/maintenance.rs` tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectRef;
    use crate::store::embeddings::{self, VecKey};
    use crate::store::memories::{self, InsertOutcome, NewMemoryInput};
    use crate::store::sources::{self, ChunkInput, SourceInput};
    use crate::store::{open_writer, projects};

    fn setup() -> (tempfile::TempDir, Connection, projects::ResolvedProject) {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef { key: "k".into(), slug: "app".into(), name: "app".into(), root: None, remote: None }).unwrap();
        (d, c, p)
    }

    fn remember(c: &Connection, p: &projects::ResolvedProject, text: &str) -> (String, i64) {
        let m = memories::prepare(NewMemoryInput { content: text.into(), ..Default::default() }, p, "h".into()).unwrap();
        match memories::insert(c, &m).unwrap() { InsertOutcome::New { id, seq } => (id, seq), o => panic!("{o:?}") }
    }

    #[test]
    fn purge_removes_a_memory_and_everything_that_points_at_it() {
        let (_d, c, p) = setup();
        let (id, seq) = remember(&c, &p, "purge me completely");
        let (other, _) = remember(&c, &p, "keep me");
        embeddings::put(&c, VecKey::Memory(seq), "fake", &[1.0, 0.0]).unwrap();
        crate::store::memories::link(&c, &other, &id, "relates_to", "t").unwrap();
        c.execute("UPDATE memories SET status = 'trashed', updated_at = '2020-01-01T00:00:00.000Z' WHERE seq = ?1", [seq]).unwrap();
        assert_eq!(trash_due(&c, "2026-01-01T00:00:00.000Z", 10).unwrap(), vec![seq]);
        let gone = purge_memories(&c, &[seq]).unwrap();
        assert_eq!(gone, vec![(id.clone(), "purge me completely".to_string())]);
        for (sql, what) in [
            ("SELECT count(*) FROM memories WHERE id = ?1", "row"),
            ("SELECT count(*) FROM links WHERE to_id = ?1 OR from_id = ?1", "links"),
            ("SELECT count(*) FROM memory_topics WHERE memory_id = ?1", "topics"),
        ] {
            let n: i64 = c.query_row(sql, [&id], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "{what}");
        }
        let fts: i64 = c.query_row("SELECT count(*) FROM memories_fts WHERE memories_fts MATCH '\"purge\"'", [], |r| r.get(0)).unwrap();
        assert_eq!(fts, 0);
        let vectors: i64 = c.query_row("SELECT count(*) FROM embeddings WHERE owner_type = 'memory' AND owner_seq = ?1", [seq], |r| r.get(0)).unwrap();
        assert_eq!(vectors, 0);
    }

    #[test]
    fn raw_retention_by_age_and_by_size_deletes_the_oldest_and_empty_sources() {
        let (_d, c, p) = setup();
        let chunks: Vec<ChunkInput> = (0..10).map(|i| ChunkInput { ord: i, text: format!("chunk {i} {}", "x".repeat(90)),
            authored_at: Some(format!("2026-01-{:02}T00:00:00.000Z", i + 1)), ..Default::default() }).collect();
        let out = sources::ingest(&c, &p, &SourceInput { uri: "s1".into(), ..Default::default() }, &chunks).unwrap();
        assert_eq!(out.inserted, 10);
        let old = chunks_older_than(&c, "2026-01-04T00:00:00.000Z", 100).unwrap();
        assert_eq!(old.len(), 3);
        assert_eq!(delete_chunks(&c, &old).unwrap(), 3);
        let before = raw_bytes(&c).unwrap();
        assert!(before >= 7 * 90);
        let oldest = oldest_chunks(&c, 2).unwrap();
        assert_eq!(oldest.len(), 2);
        delete_chunks(&c, &oldest.iter().map(|(s, _)| *s).collect::<Vec<_>>()).unwrap();
        assert!(raw_bytes(&c).unwrap() < before);
        let all: Vec<i64> = c.prepare("SELECT seq FROM chunks").unwrap().query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect();
        delete_chunks(&c, &all).unwrap();
        let sources_left: i64 = c.query_row("SELECT count(*) FROM sources", [], |r| r.get(0)).unwrap();
        assert_eq!(sources_left, 0, "a source with no chunks left is removed");
    }

    #[test]
    fn events_prune_fts_merge_and_quick_check() {
        let (_d, c, p) = setup();
        for i in 0..5 {
            remember(&c, &p, &format!("note {i}"));
        }
        c.execute("INSERT INTO events (id, ts, hlc, actor, action, target_type, target_id) VALUES ('E1', '2020-01-01T00:00:00.000Z', 'h', 'user', 'memory.edit', 'memory', 'X')", []).unwrap();
        assert_eq!(prune_events(&c, "2025-01-01T00:00:00.000Z", 500).unwrap(), 1);
        let mut rounds = 0;
        while fts_merge(&c, "memories_fts", 500).unwrap() {
            rounds += 1;
            assert!(rounds < 100, "merge terminates");
        }
        assert!(quick_check(&c).unwrap().is_empty());
        assert!(fts_merge(&c, "not_a_table", 1).is_err(), "only Alice's FTS tables");
    }
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-core store::maintenance` — Expected: FAIL to compile.

- [ ] **Step 4: Implement `store/maintenance.rs`**

```rust
//! Row-level maintenance used by the scheduler (spec §5.6). Each function does a bounded amount
//! of work so the writer's transactions stay short.

use crate::error::{AliceError, Result};
use crate::store::fts;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

const FTS_TABLES: [&str; 4] = ["memories_fts", "memories_tri", "chunks_fts", "chunks_tri"];

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()?)
}

/// Trashed memories last changed before `cutoff`, oldest first.
pub fn trash_due(conn: &Connection, cutoff: &str, limit: usize) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached(
        "SELECT seq FROM memories WHERE status = 'trashed' AND updated_at < ?1 ORDER BY seq LIMIT ?2",
    )?;
    Ok(stmt.query_map(params![cutoff, limit as i64], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?)
}

/// Delete memories for good: rows, topics, FTS rows, vectors, links both ways, mentions and
/// provenance. Returns (id, title) of each one deleted. The caller removes the index vectors.
pub fn purge_memories(conn: &Connection, seqs: &[i64]) -> Result<Vec<(String, String)>> {
    let mut gone = Vec::new();
    for &seq in seqs {
        let Some((id, title)) = conn
            .query_row("SELECT id, title FROM memories WHERE seq = ?1", [seq], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .optional()?
        else {
            continue;
        };
        fts::unindex_memory(conn, seq)?;
        conn.execute("DELETE FROM memory_topics WHERE memory_id = ?1", [&id])?;
        conn.execute("DELETE FROM links WHERE from_id = ?1 OR to_id = ?1", [&id])?;
        conn.execute("DELETE FROM mentions WHERE memory_id = ?1", [&id])?;
        conn.execute("DELETE FROM provenance WHERE memory_id = ?1", [&id])?;
        conn.execute("DELETE FROM embeddings WHERE owner_type = 'memory' AND owner_seq = ?1", [seq])?;
        conn.execute("DELETE FROM memories WHERE seq = ?1", [seq])?;
        gone.push((id, title));
    }
    Ok(gone)
}

/// Chunks authored (or stored) before `cutoff`, oldest first.
pub fn chunks_older_than(conn: &Connection, cutoff: &str, limit: usize) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached(
        "SELECT seq FROM chunks WHERE COALESCE(authored_at, created_at) < ?1 ORDER BY seq LIMIT ?2",
    )?;
    Ok(stmt.query_map(params![cutoff, limit as i64], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?)
}

/// Raw tier size as text bytes (what `storage.raw_retention = "<size>"` limits).
pub fn raw_bytes(conn: &Connection) -> Result<u64> {
    let n: Option<i64> = conn.query_row("SELECT sum(length(CAST(text AS BLOB))) FROM chunks", [], |r| r.get(0))?;
    Ok(n.unwrap_or(0) as u64)
}

pub fn oldest_chunks(conn: &Connection, limit: usize) -> Result<Vec<(i64, u64)>> {
    let mut stmt = conn.prepare_cached("SELECT seq, length(CAST(text AS BLOB)) FROM chunks ORDER BY seq LIMIT ?1")?;
    Ok(stmt
        .query_map([limit as i64], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?
        .collect::<std::result::Result<_, _>>()?)
}

/// Delete chunks with their FTS rows, vectors and provenance; sources left empty go too.
pub fn delete_chunks(conn: &Connection, seqs: &[i64]) -> Result<usize> {
    if seqs.is_empty() {
        return Ok(0);
    }
    let mut ids = Vec::new();
    let mut sources = std::collections::BTreeSet::new();
    for &seq in seqs {
        if let Some((id, src)) = conn
            .query_row("SELECT id, source_id FROM chunks WHERE seq = ?1", [seq], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .optional()?
        {
            fts::unindex_chunk(conn, seq)?;
            ids.push(id);
            sources.insert(src);
        }
    }
    for batch in seqs.chunks(500) {
        let ph = placeholders(batch.len());
        conn.execute(&format!("DELETE FROM embeddings WHERE owner_type = 'chunk' AND owner_seq IN ({ph})"), params_from_iter(batch))?;
        conn.execute(&format!("DELETE FROM chunks WHERE seq IN ({ph})"), params_from_iter(batch))?;
    }
    for batch in ids.chunks(500) {
        conn.execute(&format!("DELETE FROM provenance WHERE chunk_id IN ({})", placeholders(batch.len())), params_from_iter(batch))?;
    }
    for s in sources {
        conn.execute("DELETE FROM sources WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM chunks WHERE source_id = ?1)", [&s])?;
    }
    Ok(ids.len())
}

/// One bounded FTS5 merge step (`pages` pages of work). True while there was work to do.
pub fn fts_merge(conn: &Connection, table: &str, pages: i64) -> Result<bool> {
    if !FTS_TABLES.contains(&table) {
        return Err(AliceError::invalid(format!("{table} is not one of Alice's full-text tables")));
    }
    let before = conn.total_changes();
    conn.execute(&format!("INSERT INTO {table}({table}, rank) VALUES ('merge', ?1)"), [pages])?;
    Ok(conn.total_changes() - before > 1)
}

pub fn prune_events(conn: &Connection, before_ts: &str, limit: usize) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM events WHERE rowid IN (SELECT rowid FROM events WHERE ts < ?1 ORDER BY rowid LIMIT ?2)",
        params![before_ts, limit as i64],
    )?)
}

/// Problems `PRAGMA quick_check` reports (empty when the database is sound).
pub fn quick_check(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("PRAGMA quick_check")?;
    let rows: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
    Ok(if rows == ["ok"] { Vec::new() } else { rows })
}
```

(`fts_merge`: the `INSERT … ('merge', N)` statement itself counts as one change; more than one means segments were written. If SQLite reports exactly 1 even with work done on your version, compare `SELECT count(*) FROM {table}_data` before and after instead and keep the test.) `reviews.rs`:

```rust
/// The same question was asked before and the answer was "reject" (do not suggest it again).
pub fn was_rejected(conn: &Connection, dedupe_key: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM review_items WHERE dedupe_key = ?1 AND resolution IN ('reject', 'keep', 'keep_both'))",
        [dedupe_key], |r| r.get(0))?)
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-core store::maintenance` — Expected: pass.

- [ ] **Step 5: Backups, test first**

`crates/alice-daemon/src/backup.rs` tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn db_with_rows(dir: &Path) -> PathBuf {
        let path = dir.join("alice.db");
        let c = alice_core::store::open_writer(&path).unwrap();
        c.execute("INSERT INTO meta (key, value) VALUES ('x', 'y')", []).unwrap();
        path
    }

    #[test]
    fn a_backup_is_a_checked_copy_and_a_failure_leaves_nothing() {
        let d = tempfile::tempdir().unwrap();
        let db = db_with_rows(d.path());
        let dir = d.path().join("backups");
        let now = chrono::Utc.with_ymd_and_hms(2026, 10, 13, 3, 0, 0).unwrap();
        let b = create(&db, &dir, BackupKind::Scheduled, now).unwrap();
        assert_eq!(b.name, "alice-20261013T030000Z.db");
        let c = rusqlite::Connection::open(&b.path).unwrap();
        let v: String = c.query_row("SELECT value FROM meta WHERE key = 'x'", [], |r| r.get(0)).unwrap();
        assert_eq!(v, "y");
        assert!(create(&d.path().join("missing.db"), &dir, BackupKind::Manual, now).is_err());
        let names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec![b.name.clone()], "no temp or partial file is left");
        assert_eq!(list(&dir).unwrap()[0].kind, BackupKind::Scheduled);
    }

    #[test]
    fn rotation_keeps_seven_days_four_weeks_and_never_the_newest() {
        let d = tempfile::tempdir().unwrap();
        let now = chrono::Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        // Two backups a day for 60 days.
        for day in 0..60 {
            for hour in [1, 13] {
                let t = now - chrono::Duration::days(day) - chrono::Duration::hours(12 - hour);
                std::fs::write(d.path().join(format!("alice-{}.db", t.format("%Y%m%dT%H%M%SZ"))), b"x").unwrap();
            }
        }
        std::fs::write(d.path().join("alice-pre-migration-v2-20260801T000000Z.db"), b"x").unwrap();
        std::fs::write(d.path().join("alice-pre-migration-v1-20260701T000000Z.db"), b"x").unwrap();
        std::fs::write(d.path().join("notes.txt"), b"not a backup").unwrap();
        rotate(d.path(), now, 7, 4).unwrap();
        let kept = list(d.path()).unwrap();
        let regular: Vec<&BackupInfo> = kept.iter().filter(|b| b.kind == BackupKind::Scheduled).collect();
        // 7 daily (newest of each of the last 7 days) + newest of each of the last 4 ISO weeks
        // that are not already kept: between 7 and 11 files.
        assert!((7..=11).contains(&regular.len()), "{}", regular.len());
        assert_eq!(regular[0].created_at.date_naive(), now.date_naive(), "the newest is kept");
        let pre: Vec<&BackupInfo> = kept.iter().filter(|b| b.kind == BackupKind::PreMigration).collect();
        assert_eq!(pre.len(), 1, "older than 30 days are pruned, the newest is always kept");
        assert!(d.path().join("notes.txt").exists(), "unknown files are never touched");
        // A single old backup is never deleted.
        let e = tempfile::tempdir().unwrap();
        std::fs::write(e.path().join("alice-20250101T000000Z.db"), b"x").unwrap();
        rotate(e.path(), now, 7, 4).unwrap();
        assert_eq!(list(e.path()).unwrap().len(), 1);
    }

    #[test]
    fn legacy_pre_migration_files_are_moved_into_backups() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        std::fs::write(d.path().join("alice.pre-migration-2026-10-13T09-44-23-391Z.db"), b"x").unwrap();
        let dir = d.path().join("backups");
        assert_eq!(adopt_legacy(&home, &dir).unwrap(), 1);
        let b = list(&dir).unwrap();
        assert_eq!((b.len(), b[0].kind), (1, BackupKind::PreMigration));
    }
}
```

Implement above them:

```rust
//! Backups (spec §5.6): SQLite's online backup API on a dedicated read-only connection (one
//! step: a consistent snapshot, while WAL readers and the writer go on), checked with
//! `quick_check`, then renamed into place — a failed backup leaves no file. Rotation keeps the
//! newest backup of each of the last `daily` days and of each of the last `weekly` ISO weeks,
//! pre-migration and before-restore copies for 30 days, and never deletes the newest backup.

use crate::paths::AliceHome;
use alice_core::error::{AliceError, Result};
use alice_core::settings::BackupSettings;
use chrono::{DateTime, Datelike, NaiveDateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackupKind {
    Scheduled,
    Manual,
    PreMigration,
    BeforeRestore,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BackupInfo {
    pub name: String,
    #[schemars(with = "String")]
    pub path: PathBuf,
    pub kind: BackupKind,
    #[schemars(with = "String")]
    pub created_at: DateTime<Utc>,
    pub bytes: u64,
}

const KEEP_SPECIAL_DAYS: i64 = 30;

pub fn backup_dir(home: &AliceHome, s: &BackupSettings) -> PathBuf {
    s.dir.as_deref().map(PathBuf::from).unwrap_or_else(|| home.backups_dir())
}

fn stamp(now: DateTime<Utc>) -> String {
    now.format("%Y%m%dT%H%M%SZ").to_string()
}

fn name_for(kind: BackupKind, now: DateTime<Utc>, version: i64) -> String {
    match kind {
        // Manual backups share the scheduled ones' name and rotation.
        BackupKind::Scheduled | BackupKind::Manual => format!("alice-{}.db", stamp(now)),
        BackupKind::PreMigration => format!("alice-pre-migration-v{version}-{}.db", stamp(now)),
        BackupKind::BeforeRestore => format!("alice-before-restore-{}.db", stamp(now)),
    }
}

/// (kind, time) of a backup file name; `None` for anything else.
fn parse_name(name: &str) -> Option<(BackupKind, DateTime<Utc>)> {
    let rest = name.strip_prefix("alice-")?.strip_suffix(".db")?;
    let (kind, ts) = if let Some(r) = rest.strip_prefix("pre-migration-") {
        (BackupKind::PreMigration, r.split_once('-')?.1)
    } else if let Some(r) = rest.strip_prefix("before-restore-") {
        (BackupKind::BeforeRestore, r)
    } else {
        (BackupKind::Scheduled, rest)
    };
    let t = NaiveDateTime::parse_from_str(ts, "%Y%m%dT%H%M%SZ").ok()?.and_utc();
    Some((kind, t))
}

pub fn create(db: &Path, dir: &Path, kind: BackupKind, now: DateTime<Utc>) -> Result<BackupInfo> {
    std::fs::create_dir_all(dir)?;
    if !db.exists() {
        return Err(AliceError::NotFound(format!("database {}", db.display())));
    }
    let name = name_for(kind, now, alice_core::store::migrations::CURRENT_VERSION);
    let path = dir.join(&name);
    let tmp = dir.join(format!(".{name}.tmp"));
    let made = (|| -> Result<()> {
        let src = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        let mut dst = Connection::open(&tmp)?;
        {
            let b = rusqlite::backup::Backup::new(&src, &mut dst)?;
            // One step: the whole database inside one read transaction (a consistent snapshot;
            // a stepwise backup restarts whenever the writer commits).
            b.step(-1)?;
        }
        let problems = alice_core::store::maintenance::quick_check(&dst)?;
        if !problems.is_empty() {
            return Err(AliceError::Other(format!("the backup copy failed its check: {}", problems.join("; "))));
        }
        drop(dst);
        std::fs::File::open(&tmp)?.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if let Err(e) = made {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(BackupInfo { bytes: std::fs::metadata(&path)?.len(), name, path, kind, created_at: now })
}

/// Backups in `dir`, newest first. Other files are ignored.
pub fn list(dir: &Path) -> Result<Vec<BackupInfo>> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return Ok(out) };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some((kind, created_at)) = parse_name(&name) {
            out.push(BackupInfo { bytes: e.metadata().map(|m| m.len()).unwrap_or(0), path: e.path(), name, kind, created_at });
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.name.cmp(&a.name)));
    Ok(out)
}

/// Delete what the policy does not keep; returns the deleted paths.
pub fn rotate(dir: &Path, now: DateTime<Utc>, daily: u32, weekly: u32) -> Result<Vec<PathBuf>> {
    let all = list(dir)?;
    let mut keep = std::collections::HashSet::new();
    let regular: Vec<&BackupInfo> = all.iter().filter(|b| matches!(b.kind, BackupKind::Scheduled | BackupKind::Manual)).collect();
    if let Some(newest) = regular.first() {
        keep.insert(newest.path.clone());
    }
    let mut days = Vec::new();
    let mut weeks = Vec::new();
    for b in &regular {
        let day = b.created_at.date_naive();
        if days.len() < daily as usize && !days.contains(&day) {
            days.push(day);
            keep.insert(b.path.clone());
        }
        let week = (b.created_at.iso_week().year(), b.created_at.iso_week().week());
        if weeks.len() < weekly as usize && !weeks.contains(&week) {
            weeks.push(week);
            keep.insert(b.path.clone());
        }
    }
    for kind in [BackupKind::PreMigration, BackupKind::BeforeRestore] {
        let of_kind: Vec<&BackupInfo> = all.iter().filter(|b| b.kind == kind).collect();
        if let Some(newest) = of_kind.first() {
            keep.insert(newest.path.clone());
        }
        for b in of_kind {
            if now - b.created_at < chrono::Duration::days(KEEP_SPECIAL_DAYS) {
                keep.insert(b.path.clone());
            }
        }
    }
    let mut deleted = Vec::new();
    for b in &all {
        if !keep.contains(&b.path) {
            std::fs::remove_file(&b.path)?;
            deleted.push(b.path.clone());
        }
    }
    Ok(deleted)
}

/// M1 wrote pre-migration backups next to `alice.db` as `alice.pre-migration-<stamp>.db`.
pub fn adopt_legacy(home: &AliceHome, dir: &Path) -> Result<usize> {
    std::fs::create_dir_all(dir)?;
    let mut n = 0;
    for e in std::fs::read_dir(home.root())?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(ts) = name.strip_prefix("alice.pre-migration-").and_then(|r| r.strip_suffix(".db")) else { continue };
        // e.g. 2026-10-13T09-44-23-391Z
        let t = NaiveDateTime::parse_from_str(&ts[..19.min(ts.len())], "%Y-%m-%dT%H-%M-%S")
            .map(|t| t.and_utc())
            .unwrap_or_else(|_| Utc::now());
        std::fs::rename(e.path(), dir.join(format!("alice-pre-migration-v1-{}.db", stamp(t))))?;
        n += 1;
    }
    Ok(n)
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-daemon backup::` — Expected: pass.

- [ ] **Step 6: Writer ops for maintenance**

`writer.rs`:

```rust
    SetMeta { key: String, value: String },
    /// Delete trashed memories for good (returns their seqs; the caller drops their vectors).
    PurgeMemories { seqs: Vec<i64> },
    /// Delete raw chunks (retention); the caller drops their vectors.
    DeleteChunks { seqs: Vec<i64>, reason: String },
    RequeueJudge { max_attempts: u32 },
    /// One bounded FTS merge step; `WriteOutcome::Flag(true)` while there was work.
    FtsMerge { table: String, pages: i64 },
    PruneEvents { before: String, limit: usize },
    CreateReview(NewReview),
```

with `WriteOutcome::{Purged(Vec<i64>), Count(usize), Flag(bool)}`. `PurgeMemories` records one `memory.purge` event per memory (actor `system`, before `{"id", "title"}`, after `None`); `DeleteChunks` records one `raw.retention` event (target `raw`, after `{"deleted": n, "reason": …}`); `CreateReview` creates and pushes `StreamItem::Review`. `SetMeta`: `INSERT INTO meta … ON CONFLICT(key) DO UPDATE SET value = excluded.value`.

- [ ] **Step 7: The scheduler, test first**

`crates/alice-daemon/src/maintenance.rs` tests (a rig like `worker.rs`'s: writer, reader pool, fake index, settings handle in a temp home):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_are_due_by_their_period_and_run_only_when_idle() {
        let r = rig();
        let now = chrono::Utc::now();
        assert_eq!(r.sched.due(now).len(), Job::ALL.len(), "never run: all due");
        for job in [Job::TrashPurge, Job::JudgeRetry] {
            let report = r.sched.run_job(job, now, false);
            assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        }
        let due = r.sched.due(now);
        assert!(!due.contains(&Job::TrashPurge) && !due.contains(&Job::JudgeRetry));
        // Each job comes back after its own period, not before.
        assert!(!r.sched.due(now + chrono::Duration::hours(23)).contains(&Job::TrashPurge));
        assert!(r.sched.due(now + chrono::Duration::days(1) + chrono::Duration::seconds(1)).contains(&Job::TrashPurge));
        assert!(!r.sched.due(now + chrono::Duration::minutes(30)).contains(&Job::JudgeRetry), "hourly, not sooner");
        assert!(r.sched.due(now + chrono::Duration::hours(1) + chrono::Duration::seconds(1)).contains(&Job::JudgeRetry));
        // Busy: a request in flight means nothing runs on the timer.
        let _g = r.activity.enter();
        assert!(!r.sched.idle_enough(&r.settings.get().maintenance));
    }

    #[test]
    fn a_long_job_yields_when_a_request_arrives() {
        let r = rig();
        r.insert_trashed_memories(1_200, "2020-01-01T00:00:00.000Z");
        {
            // A request is in flight for the whole run: the job does one 500-row slice, sees
            // it, and stops there (deterministic: no timing involved).
            let _g = r.activity.enter();
            let report = r.sched.run_job(Job::TrashPurge, chrono::Utc::now(), false);
            assert_eq!(report.outcome, JobOutcome::Paused, "{report:?}");
            assert_eq!(r.count("SELECT count(*) FROM memories"), 700, "exactly one slice ran");
            assert_eq!(r.sched.meta("maint.trash_purge.last_run"), None, "a paused job is not a run");
        }
        let forced = r.sched.run_job(Job::TrashPurge, chrono::Utc::now(), true);
        assert_eq!(forced.outcome, JobOutcome::Done);
        assert_eq!(r.count("SELECT count(*) FROM memories"), 0);
    }

    #[test]
    fn index_check_rebuilds_a_drifted_index() {
        let r = rig();
        r.insert_embedded_memories(30);
        r.index_remove_one();
        let report = r.sched.run_job(Job::IndexCheck, chrono::Utc::now(), true);
        assert!(report.detail.contains("rebuild"), "{report:?}");
        r.wait_index_len(30);
    }

    #[test]
    fn merge_suggestions_cluster_near_duplicates_once() {
        let r = rig();
        r.insert_memories_with_vectors(&[("A", [1.0, 0.0, 0.0]), ("B", [0.99, 0.05, 0.0]), ("C", [0.0, 1.0, 0.0])]);
        r.sched.run_job(Job::MergeSuggestions, chrono::Utc::now(), true);
        assert_eq!(r.count("SELECT count(*) FROM review_items WHERE type = 'merge' AND status = 'open'"), 1);
        r.sched.run_job(Job::MergeSuggestions, chrono::Utc::now() + chrono::Duration::days(8), true);
        assert_eq!(r.count("SELECT count(*) FROM review_items WHERE type = 'merge'"), 1, "asked once");
    }

    #[test]
    fn backups_run_rotate_and_adopt_old_files() {
        let r = rig();
        std::fs::write(r.home.root().join("alice.pre-migration-2026-10-13T09-44-23-391Z.db"), b"x").unwrap();
        let report = r.sched.run_job(Job::Backup, chrono::Utc::now(), true);
        assert_eq!(report.outcome, JobOutcome::Done, "{report:?}");
        let backups = crate::backup::list(&r.home.backups_dir()).unwrap();
        assert!(backups.iter().any(|b| b.kind == crate::backup::BackupKind::Scheduled));
        assert!(backups.iter().any(|b| b.kind == crate::backup::BackupKind::PreMigration));
    }
}
```

(The rig helpers — `rig()`, `insert_trashed_memories`, `insert_embedded_memories`, `index_remove_one`, `wait_index_len`, `insert_memories_with_vectors` (3-dimension vectors with a `VectorIndex::for_model("fake", 3)`), `count` — go in the test module; they write through the rig's writer with direct SQL where the setup needs past timestamps.)

Implement:

```rust
//! Maintenance (spec §5.6): jobs run on a low-priority thread when the daemon has had no
//! requests for `maintenance.idle_secs`, in short slices, and stop between slices as soon as a
//! request arrives (the next idle period continues them). Last runs live in `meta`.

use crate::backup::{self, BackupKind};
use crate::config::SettingsHandle;
use crate::paths::AliceHome;
use crate::service::IndexSlot;
use crate::writer::{WriteOp, WriteOutcome, WriterHandle};
use alice_core::settings::{MaintenanceSettings, Retention};
use alice_core::store::embeddings::{self, VecKey};
use alice_core::store::{ReaderPool, maintenance as m, reviews};
use chrono::{DateTime, Utc};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// When requests last came, and how many are running.
pub struct Activity {
    epoch: Instant,
    last_ms: AtomicI64,
    inflight: AtomicUsize,
}

pub struct ActivityGuard<'a>(&'a Activity);

impl Drop for ActivityGuard<'_> {
    fn drop(&mut self) {
        self.0.inflight.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}

impl Activity {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { epoch: Instant::now(), last_ms: AtomicI64::new(0), inflight: AtomicUsize::new(0) })
    }
    fn touch(&self) {
        self.last_ms.store(self.epoch.elapsed().as_millis() as i64, Ordering::SeqCst);
    }
    /// Held for the length of a request.
    pub fn enter(&self) -> ActivityGuard<'_> {
        self.inflight.fetch_add(1, Ordering::SeqCst);
        self.touch();
        ActivityGuard(self)
    }
    pub fn idle_for(&self) -> Duration {
        if self.inflight.load(Ordering::SeqCst) > 0 {
            return Duration::ZERO;
        }
        let now = self.epoch.elapsed().as_millis() as i64;
        Duration::from_millis((now - self.last_ms.load(Ordering::SeqCst)).max(0) as u64)
    }
    /// A request started or ran since `since` (an `epoch`-relative mark from `mark`).
    pub fn active_since(&self, since: i64) -> bool {
        self.inflight.load(Ordering::SeqCst) > 0 || self.last_ms.load(Ordering::SeqCst) > since
    }
    pub fn mark(&self) -> i64 {
        self.epoch.elapsed().as_millis() as i64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Job {
    Backup,
    QuickCheck,
    IndexCheck,
    FtsOptimize,
    RawRetention,
    TrashPurge,
    JudgeRetry,
    MergeSuggestions,
    EventsPrune,
}

impl Job {
    pub const ALL: [Job; 9] = [Job::JudgeRetry, Job::TrashPurge, Job::RawRetention, Job::EventsPrune, Job::QuickCheck,
        Job::IndexCheck, Job::Backup, Job::FtsOptimize, Job::MergeSuggestions];
    pub fn name(self) -> &'static str {
        match self {
            Job::Backup => "backup", Job::QuickCheck => "quick_check", Job::IndexCheck => "index_check",
            Job::FtsOptimize => "fts_optimize", Job::RawRetention => "raw_retention", Job::TrashPurge => "trash_purge",
            Job::JudgeRetry => "judge_retry", Job::MergeSuggestions => "merge_suggestions", Job::EventsPrune => "events_prune",
        }
    }
    pub fn parse(s: &str) -> Option<Job> {
        Job::ALL.into_iter().find(|j| j.name() == s)
    }
    pub fn every(self) -> Duration {
        const H: u64 = 3_600;
        Duration::from_secs(match self {
            Job::JudgeRetry => H,
            Job::FtsOptimize | Job::MergeSuggestions => 7 * 24 * H,
            _ => 24 * H,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobOutcome {
    Done,
    /// A request arrived; the job continues in the next idle period.
    Paused,
    Failed,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct JobReport {
    pub job: Job,
    pub outcome: JobOutcome,
    pub detail: String,
    pub took_ms: u64,
}

pub struct MaintenanceCtx {
    pub home: AliceHome,
    pub readers: Arc<ReaderPool>,
    pub writer: WriterHandle,
    pub index: Arc<IndexSlot>,
    pub settings: Arc<SettingsHandle>,
    pub activity: Arc<Activity>,
    pub model: Arc<dyn Fn() -> String + Send + Sync>,
    pub dim: usize,
    pub judge_wake: crossbeam_channel::Sender<()>,
}

pub struct Scheduler {
    ctx: MaintenanceCtx,
}

/// Rows per write in purges and retention.
const SLICE: usize = 500;

enum Step {
    More,
    Finished(String),
}

impl Scheduler {
    pub fn new(ctx: MaintenanceCtx) -> Self {
        Self { ctx }
    }

    fn meta(&self, key: &str) -> Option<String> {
        self.ctx.readers.with(|c| m::get_meta(c, key)).ok().flatten()
    }

    /// Committed before it returns, so `due` sees it at once.
    fn set_meta(&self, key: &str, value: &str) {
        if let Err(e) = self.ctx.writer.submit_blocking(WriteOp::SetMeta { key: key.into(), value: value.into() }) {
            tracing::warn!(error = %e, key, "recording a maintenance run failed");
        }
    }

    pub fn due(&self, now: DateTime<Utc>) -> Vec<Job> {
        Job::ALL
            .into_iter()
            .filter(|j| {
                self.meta(&format!("maint.{}.last_run", j.name()))
                    .and_then(|t| DateTime::parse_from_rfc3339(&t).ok())
                    .is_none_or(|t| now - t.with_timezone(&Utc) >= chrono::Duration::from_std(j.every()).unwrap_or_default())
            })
            .collect()
    }

    pub fn idle_enough(&self, s: &MaintenanceSettings) -> bool {
        s.enabled && self.ctx.activity.idle_for() >= Duration::from_secs(s.idle_secs)
    }

    /// Run `job` in slices. Without `force` it stops (Paused) when a request arrives; `force`
    /// (the CLI's `maintenance run`) runs it to the end.
    pub fn run_job(&self, job: Job, now: DateTime<Utc>, force: bool) -> JobReport {
        let started = Instant::now();
        let mark = self.ctx.activity.mark();
        let mut outcome = JobOutcome::Done;
        let mut detail = String::new();
        loop {
            match self.step(job, now) {
                Ok(Step::Finished(d)) => {
                    detail = d;
                    break;
                }
                Ok(Step::More) => {
                    if !force && self.ctx.activity.active_since(mark) {
                        outcome = JobOutcome::Paused;
                        detail = "paused: a request arrived".into();
                        break;
                    }
                }
                Err(e) => {
                    outcome = JobOutcome::Failed;
                    detail = e.to_string();
                    break;
                }
            }
        }
        let stamp = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        if outcome != JobOutcome::Paused {
            self.set_meta(&format!("maint.{}.last_run", job.name()), &stamp);
        }
        match outcome {
            JobOutcome::Done => self.set_meta(&format!("maint.{}.last_ok", job.name()), &stamp),
            JobOutcome::Failed => {
                tracing::warn!(job = job.name(), error = %detail, "maintenance job failed");
                self.set_meta(&format!("maint.{}.last_error", job.name()), &format!("{stamp} {detail}"));
            }
            JobOutcome::Paused => {}
        }
        JobReport { job, outcome, detail, took_ms: started.elapsed().as_millis() as u64 }
    }

    fn step(&self, job: Job, now: DateTime<Utc>) -> alice_core::Result<Step> {
        let s = self.ctx.settings.get();
        let w = &self.ctx.writer;
        let ts = |t: DateTime<Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        match job {
            Job::TrashPurge => {
                let cutoff = ts(now - chrono::Duration::days(i64::from(s.maintenance.trash_days)));
                let seqs = self.ctx.readers.with(|c| m::trash_due(c, &cutoff, SLICE))?;
                if seqs.is_empty() {
                    return Ok(Step::Finished("nothing to purge".into()));
                }
                if let WriteOutcome::Purged(done) = w.submit_blocking(WriteOp::PurgeMemories { seqs })?
                    && let Some(idx) = self.ctx.index.read().clone()
                {
                    for seq in done {
                        let _ = idx.remove(VecKey::Memory(seq));
                    }
                }
                Ok(Step::More)
            }
            Job::RawRetention => {
                let seqs: Vec<i64> = match s.storage.raw_retention {
                    Retention::Forever => return Ok(Step::Finished("retention is forever".into())),
                    Retention::Age(age) => {
                        let cutoff = ts(now - chrono::Duration::from_std(age).unwrap_or_default());
                        self.ctx.readers.with(|c| m::chunks_older_than(c, &cutoff, SLICE))?
                    }
                    Retention::Size(limit) => {
                        let (total, oldest) = self.ctx.readers.with(|c| Ok((m::raw_bytes(c)?, m::oldest_chunks(c, SLICE)?)))?;
                        let mut over = total.saturating_sub(limit);
                        oldest.into_iter().take_while(|(_, b)| { let take = over > 0; over = over.saturating_sub(*b); take }).map(|(s, _)| s).collect()
                    }
                };
                if seqs.is_empty() {
                    return Ok(Step::Finished("raw tier within retention".into()));
                }
                w.submit_blocking(WriteOp::DeleteChunks { seqs: seqs.clone(), reason: s.storage.raw_retention.to_string() })?;
                if let Some(idx) = self.ctx.index.read().clone() {
                    idx.remove_chunks(&seqs)?;
                }
                Ok(Step::More)
            }
            Job::EventsPrune => {
                let before = ts(now - chrono::Duration::days(i64::from(s.maintenance.events_days)));
                match w.submit_blocking(WriteOp::PruneEvents { before, limit: SLICE })? {
                    WriteOutcome::Count(n) if n > 0 => Ok(Step::More),
                    _ => Ok(Step::Finished("events pruned".into())),
                }
            }
            Job::JudgeRetry => {
                w.submit_blocking(WriteOp::RequeueJudge { max_attempts: 5 })?;
                let _ = self.ctx.judge_wake.try_send(());
                Ok(Step::Finished("judge retries queued".into()))
            }
            Job::FtsOptimize => {
                for table in ["memories_fts", "memories_tri", "chunks_fts", "chunks_tri"] {
                    if let WriteOutcome::Flag(true) = w.submit_blocking(WriteOp::FtsMerge { table: table.into(), pages: 500 })? {
                        return Ok(Step::More);
                    }
                }
                Ok(Step::Finished("full-text indexes merged".into()))
            }
            Job::QuickCheck => {
                let c = rusqlite::Connection::open_with_flags(self.ctx.home.db(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                let problems = m::quick_check(&c)?;
                if problems.is_empty() {
                    return Ok(Step::Finished("quick_check ok".into()));
                }
                let reason = format!("PRAGMA quick_check found damage ({})", problems.first().cloned().unwrap_or_default());
                self.ctx.writer.read_only.set(reason.clone());
                Err(alice_core::AliceError::Other(reason))
            }
            Job::IndexCheck => {
                let model = (self.ctx.model)();
                let stored = self.ctx.readers.with(|c| embeddings::count(c, &model))?;
                let Some(idx) = self.ctx.index.read().clone() else {
                    return Ok(Step::Finished("index is being rebuilt".into()));
                };
                if idx.len() == stored {
                    return Ok(Step::Finished(format!("index holds all {stored} vectors")));
                }
                *self.ctx.index.write() = None;
                crate::service::spawn_rebuild(self.ctx.index.clone(), self.ctx.home.db(), model, self.ctx.dim)?;
                Ok(Step::Finished(format!("index held {} of {stored} vectors; rebuild started", idx.len())))
            }
            Job::Backup => {
                let dir = backup::backup_dir(&self.ctx.home, &s.backup);
                let adopted = backup::adopt_legacy(&self.ctx.home, &dir)?;
                let b = backup::create(&self.ctx.home.db(), &dir, BackupKind::Scheduled, now)?;
                let deleted = backup::rotate(&dir, now, s.backup.daily, s.backup.weekly)?;
                Ok(Step::Finished(format!("{} ({} bytes); {} old backups removed; {adopted} legacy files moved", b.name, b.bytes, deleted.len())))
            }
            Job::MergeSuggestions => self.merge_suggestions(f64::from(s.judge.cluster_similarity)),
        }
    }

    /// Weekly: groups of live memories in one project whose vectors are at least
    /// `judge.cluster_similarity` apart become one `merge` review item (asked once).
    fn merge_suggestions(&self, threshold: f64) -> alice_core::Result<Step> {
        const PAGE: i64 = 2_000;
        let Some(idx) = self.ctx.index.read().clone() else { return Ok(Step::Finished("index not ready".into())) };
        let model = (self.ctx.model)();
        let cursor: i64 = self.meta("maint.merge.cursor").and_then(|v| v.parse().ok()).unwrap_or(0);
        let rows: Vec<(i64, String, String, u8)> = self.ctx.readers.with(|c| {
            let mut stmt = c.prepare("SELECT seq, id, project_id, importance FROM memories WHERE seq > ?1 AND status IN ('active','pending') ORDER BY seq LIMIT ?2")?;
            Ok(stmt.query_map(rusqlite::params![cursor, PAGE], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<std::result::Result<_, _>>()?)
        })?;
        if rows.is_empty() {
            self.set_meta("maint.merge.cursor", "0");
            return Ok(Step::Finished("merge suggestions done".into()));
        }
        let mut made = 0;
        for (seq, id, project, _) in &rows {
            let Some((_, v)) = self.ctx.readers.with(|c| embeddings::get_many(c, &model, &[VecKey::Memory(*seq)]))?.pop() else { continue };
            let near: Vec<i64> = idx.search(&v, 10, alice_core::vector::KeyFilter::Memories)?
                .into_iter()
                .filter_map(|(k, s)| match k { VecKey::Memory(o) if o != *seq && f64::from(s) >= threshold => Some(o), _ => None })
                .collect();
            if near.is_empty() {
                continue;
            }
            let members: Vec<(String, String, u8, String)> = self.ctx.readers.with(|c| {
                let mut out = Vec::new();
                for o in &near {
                    if let Some(r) = c.query_row(
                        "SELECT id, project_id, importance, created_at FROM memories WHERE seq = ?1 AND status IN ('active','pending')",
                        [o], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?
                    {
                        out.push(r);
                    }
                }
                Ok(out)
            })?;
            let mut group: Vec<String> = members.into_iter().filter(|(_, p, _, _)| p == project).map(|(i, _, _, _)| i).collect();
            if group.is_empty() {
                continue;
            }
            group.push(id.clone());
            group.sort();
            group.dedup();
            let key = format!("merge:{}", group.join(":"));
            if self.ctx.readers.with(|c| reviews::was_rejected(c, &key))? {
                continue;
            }
            // The survivor: the oldest (smallest id, ULIDs sort by time).
            let survivor = group[0].clone();
            let others: Vec<String> = group[1..].to_vec();
            self.ctx.writer.submit_blocking(WriteOp::CreateReview(alice_core::store::reviews::NewReview {
                kind: alice_core::store::reviews::ReviewType::Merge,
                payload: serde_json::json!({"survivor": survivor, "others": others, "p": threshold, "provider": "cluster"}),
                confidence: Some(threshold),
                dedupe_key: Some(key),
            }))?;
            made += 1;
        }
        let last = rows.last().map(|r| r.0).unwrap_or(cursor);
        self.set_meta("maint.merge.cursor", &last.to_string());
        tracing::debug!(suggested = made, up_to_seq = last, "merge suggestions");
        Ok(Step::More)
    }

    /// Every 5 s: when idle long enough, run the due jobs one after another. Takes the shared
    /// `Arc` so `Service::run_job` can run jobs on demand on the same scheduler.
    pub fn spawn(self: Arc<Self>, stop: Arc<AtomicBool>) -> JoinHandle<()> {
        std::thread::Builder::new()
            .name("alice-maintenance".into())
            .spawn(move || {
                lower_priority();
                while !stop.load(Ordering::Relaxed) {
                    let settings = self.ctx.settings.get();
                    if self.idle_enough(&settings.maintenance) && self.ctx.writer.read_only.reason().is_none() {
                        for job in self.due(Utc::now()) {
                            if stop.load(Ordering::Relaxed) || !self.idle_enough(&settings.maintenance) {
                                break;
                            }
                            let r = self.run_job(job, Utc::now(), false);
                            tracing::info!(job = job.name(), outcome = ?r.outcome, took_ms = r.took_ms, detail = %r.detail, "maintenance");
                        }
                    }
                    for _ in 0..50 {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            })
            .expect("spawn maintenance thread")
    }
}

/// Best effort: below normal priority so maintenance never competes with requests.
fn lower_priority() {
    #[cfg(target_os = "linux")]
    // SAFETY: changes only this thread's nice value.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, 10);
    }
    #[cfg(target_os = "macos")]
    // SAFETY: sets this thread's QoS class.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
    }
}
```

(`ReadOnlyState::set` becomes `pub(crate)` already; `service::spawn_rebuild` becomes `pub(crate)`. Use `rusqlite::OptionalExtension` in `merge_suggestions`. The merge-suggestion loop deliberately suggests per seed memory; the `dedupe_key` stops the same group being asked twice.)

`service.rs`: `activity: Arc<Activity>`; every public request method (`remember`, `recall`, `context`, `get`, `update`, `facts`, `assert_fact`, `ingest`, `events`, `undo`, reviews…) starts with `let _busy = self.activity.enter();`. Build `let scheduler = Arc::new(Scheduler::new(MaintenanceCtx { … }))` in `start`, keep it in `Service.scheduler`, and `scheduler.clone().spawn(stop.clone())`; join it in `shutdown` first (it may be mid-job: it stops between slices because `stop` makes `spawn`'s loop exit — also check `stop` inside `run_job`'s loop to break early with `Paused`). Add:

```rust
    pub async fn run_job(&self, job: Job) -> Result<JobReport> {
        let sched = self.scheduler.clone(); // Arc<Scheduler> kept for on-demand runs
        tokio::task::spawn_blocking(move || sched.run_job(job, chrono::Utc::now(), true)).await.map_err(join_err)
    }
    pub async fn backup_now(&self) -> Result<BackupInfo> {
        let (db, dir) = (self.home.db(), crate::backup::backup_dir(&self.home, &self.settings().backup));
        tokio::task::spawn_blocking(move || crate::backup::create(&db, &dir, BackupKind::Manual, chrono::Utc::now())).await.map_err(join_err)?
    }
    pub async fn backups(&self) -> Result<Vec<BackupInfo>> {
        crate::backup::list(&crate::backup::backup_dir(&self.home, &self.settings().backup))
    }
    pub async fn maintenance_status(&self) -> Result<Vec<JobStatus>> {
        self.read(|c| Job::ALL.iter().map(|j| Ok(JobStatus {
            job: j.name().into(), every_secs: j.every().as_secs(),
            last_run: m::get_meta(c, &format!("maint.{}.last_run", j.name()))?,
            last_ok: m::get_meta(c, &format!("maint.{}.last_ok", j.name()))?,
            last_error: m::get_meta(c, &format!("maint.{}.last_error", j.name()))?,
        })).collect()).await
    }
```

(So `Scheduler` is shared: keep it in an `Arc` and give `spawn` an `Arc<Self>` receiver.) HTTP routes and client methods as listed in Interfaces; CLI:

```rust
    /// Backups of the database (`now` makes one; `list` shows them).
    #[command(subcommand)]
    Backup(cmd::BackupCmd),
    /// Maintenance jobs: when they last ran, or run one now.
    #[command(subcommand)]
    Maintenance(cmd::MaintenanceCmd),
```

```rust
#[derive(Subcommand)]
pub enum BackupCmd { Now, List }

#[derive(Subcommand)]
pub enum MaintenanceCmd {
    Status,
    /// backup, quick_check, index_check, fts_optimize, raw_retention, trash_purge, judge_retry, merge_suggestions, events_prune
    Run { job: String },
}
```

Process test (unix) in `crates/alice/tests/cli.rs`: `alice backup now` then `alice --json backup list` has one entry of kind `manual`/`scheduled` whose file exists; `alice maintenance run trash_purge` prints `done`; `alice maintenance run nope` fails naming the jobs; `alice --json maintenance status` lists every job in `Job::ALL`.

Run: `nice -n 15 cargo test -j 6 --workspace` — Expected: pass.

- [ ] **Step 8: Measure the jobs at 1M chunks**

On the Task 1 bench data, with `ALICE_HOME` exported to it for **every** command of this step (the daemon, `alice maintenance run …` and the per-second `alice recall`; otherwise those CLI calls would auto-start the M2 binary on the real `~/.alice`). Use the real model, linked as in Task 6 Step 7: the data's vectors belong to it, and a fake embedder would not match them.

```bash
export S=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2 ALICE_HOME=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2/data-1m-prefill
[ -e "$ALICE_HOME/models" ] || ln -s "$HOME/.alice/models" "$ALICE_HOME/models"
nice -n 15 cargo build --release -j 6 -p alice-memory
ALICE_STORAGE_RAW_RETENTION=90d nice -n 15 target/release/alice daemon run & PID=$!
until target/release/alice status >/dev/null 2>&1; do sleep 1; done
( while kill -0 $PID 2>/dev/null; do /usr/bin/time -p target/release/alice recall "release checklist" >/dev/null 2>>$S/recall-times.txt; sleep 1; done ) & LOOP=$!
for j in backup quick_check index_check fts_optimize raw_retention; do /usr/bin/time -p target/release/alice maintenance run $j; done
target/release/alice daemon stop; wait $LOOP
pgrep -fl "alice daemon run" || true
```

Record each job's duration and the recall worst case (from `$S/recall-times.txt`, `real` lines) in `docs/benchmarks/m2.md` → "Maintenance at 1M chunks". The gate: recall during a job stays under 200 ms p95 (backup and quick_check read through the OS cache on their own connections; the writer is only held for 500-row slices). The first start migrates this data to schema v3 and the backup job writes a full copy under `$S/data-1m-prefill/backups/` (several GB; Task 18 deletes `$S/data-*`).

- [ ] **Step 9: Commit**

```bash
nice -n 15 cargo fmt --all && nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
git add Cargo.toml Cargo.lock crates docs/benchmarks/m2.md
git commit -m "feat: idle maintenance scheduler with backups and rotation, checks, retention, purges and merge suggestions

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 15: Restore with a verified round trip

Spec §5.6: "Restore with `alice restore [backup]`, which stops writes, swaps files and rebuilds indexes." M2 acceptance: "restore round-trip verified". The CLI does it, because the daemon must not be running while `alice.db` is replaced: it verifies the backup (a SQLite file that passes `quick_check`, an Alice schema not newer than this build), stops the daemon (agents' writes go to the spool meanwhile), takes the daemon lock so no daemon can start during the swap, moves the current database (with its WAL) to `backups/alice-before-restore-<stamp>.db`, copies the backup into place, deletes the vector index files, releases the lock and starts the daemon, which rebuilds the index and drains the spool into the restored database.

**Files:**
- Create: `crates/alice-daemon/src/restore.rs`; modify `lib.rs`
- Modify: `crates/alice-daemon/src/daemon.rs` (`pub fn try_lock_home(home) -> io::Result<Option<File>>`, no waiting)
- Modify: `crates/alice/src/main.rs`, `crates/alice/src/cmd.rs` (`alice restore`; `stop` split into `stop_daemon` + printing)
- Test: `restore.rs` unit tests, `crates/alice/tests/cli.rs` (round trip)

**Interfaces:**
- Consumes: Task 14 (`backup::{list, backup_dir, BackupKind}`), Task 5 (`config::load_effective`), M1 `cmd::stop` (split here into a quiet `stop_daemon` that waits until the lock is free, and the printing `stop`), `Launcher`, `DaemonClient::connect`.
- Produces:
  - `alice_daemon::restore::{VerifiedBackup { path, schema_version: i64, memories: u64, chunks: u64 }, RestoreReport { restored_from: String, saved_current_as: Option<String>, memories: u64, chunks: u64 }, verify(path: &Path) -> Result<VerifiedBackup>, find_backup(home: &AliceHome, dir: &Path, name_or_path: Option<&str>) -> Result<PathBuf>, swap(home: &AliceHome, dir: &Path, backup: &Path, now: DateTime<Utc>) -> Result<RestoreReport>}`
  - `alice_daemon::daemon::try_lock_home(home: &AliceHome) -> std::io::Result<Option<std::fs::File>>`
  - CLI: `alice restore [<backup name or path>] [--yes]` (default: the newest backup; asks for confirmation on a terminal; `--yes` required otherwise).

- [ ] **Step 1: Read the files this task modifies, as they are now.**

- [ ] **Step 2: Write the failing tests**

`crates/alice-daemon/src/restore.rs` tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn home_with_db() -> (tempfile::TempDir, AliceHome) {
        let d = tempfile::tempdir().unwrap();
        let h = AliceHome::new(d.path().to_path_buf());
        h.ensure_dirs().unwrap();
        let c = alice_core::store::open_writer(&h.db()).unwrap();
        c.execute("INSERT INTO meta (key, value) VALUES ('which', 'current')", []).unwrap();
        (d, h)
    }

    #[test]
    fn verify_accepts_a_backup_and_refuses_damage_and_newer_schemas() {
        let (d, h) = home_with_db();
        let b = crate::backup::create(&h.db(), &h.backups_dir(), crate::backup::BackupKind::Manual, Utc::now()).unwrap();
        let v = verify(&b.path).unwrap();
        assert_eq!(v.schema_version, alice_core::store::migrations::CURRENT_VERSION);
        let bytes = std::fs::read(&b.path).unwrap();
        let truncated = d.path().join("half.db");
        std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
        assert!(verify(&truncated).is_err(), "truncated");
        let text = d.path().join("notes.db");
        std::fs::write(&text, b"just some text").unwrap();
        assert!(verify(&text).unwrap_err().to_string().contains("not an Alice backup"));
        let newer = d.path().join("newer.db");
        std::fs::copy(&b.path, &newer).unwrap();
        rusqlite::Connection::open(&newer).unwrap().execute("INSERT INTO schema_migrations VALUES (99, 'x')", []).unwrap();
        assert!(verify(&newer).unwrap_err().to_string().contains("newer"));
    }

    #[test]
    fn swap_needs_the_lock_saves_the_current_database_and_drops_the_index() {
        let (_d, h) = home_with_db();
        let dir = h.backups_dir();
        let b = crate::backup::create(&h.db(), &dir, crate::backup::BackupKind::Manual, Utc::now()).unwrap();
        rusqlite::Connection::open(h.db()).unwrap().execute("UPDATE meta SET value = 'later' WHERE key = 'which'", []).unwrap();
        std::fs::write(h.root().join("vectors/fake.memories.usearch"), b"x").unwrap();
        let held = crate::daemon::try_lock_home(&h).unwrap().expect("free");
        let err = swap(&h, &dir, &b.path, Utc::now()).unwrap_err().to_string();
        assert!(err.contains("still running"), "{err}");
        drop(held);
        let r = swap(&h, &dir, &b.path, Utc::now()).unwrap();
        let which: String = rusqlite::Connection::open(h.db()).unwrap().query_row("SELECT value FROM meta WHERE key = 'which'", [], |r| r.get(0)).unwrap();
        assert_eq!(which, "current", "the backup's content is in place");
        let saved = dir.join(r.saved_current_as.unwrap());
        let before: String = rusqlite::Connection::open(&saved).unwrap().query_row("SELECT value FROM meta WHERE key = 'which'", [], |r| r.get(0)).unwrap();
        assert_eq!(before, "later", "the replaced database was kept");
        assert!(!h.root().join("vectors/fake.memories.usearch").exists(), "the index is rebuilt on start");
    }

    #[test]
    fn find_backup_takes_a_name_a_path_or_the_newest() {
        let (_d, h) = home_with_db();
        let dir = h.backups_dir();
        assert!(find_backup(&h, &dir, None).unwrap_err().to_string().contains("alice backup now"));
        let a = crate::backup::create(&h.db(), &dir, crate::backup::BackupKind::Manual, Utc::now() - chrono::Duration::days(1)).unwrap();
        let b = crate::backup::create(&h.db(), &dir, crate::backup::BackupKind::Manual, Utc::now()).unwrap();
        assert_eq!(find_backup(&h, &dir, None).unwrap(), b.path);
        assert_eq!(find_backup(&h, &dir, Some(&a.name)).unwrap(), a.path);
        assert_eq!(find_backup(&h, &dir, Some(a.path.to_str().unwrap())).unwrap(), a.path);
        assert!(find_backup(&h, &dir, Some("nope.db")).is_err());
    }
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-daemon restore::` — Expected: FAIL to compile.

- [ ] **Step 3: Implement**

`daemon.rs`:

```rust
/// The single-instance lock if it is free right now (no waiting); `None` when a daemon holds it.
pub fn try_lock_home(home: &AliceHome) -> std::io::Result<Option<File>> {
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(home.lock_file())?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}
```

`restore.rs`:

```rust
//! Restore (spec §5.6): verify a backup, then — while holding the daemon lock, so no daemon
//! runs — keep the current database as `alice-before-restore-<stamp>.db`, put the backup in
//! its place and drop the vector index (rebuilt at the next start). Never opens `alice.db`.

use crate::paths::AliceHome;
use alice_core::error::{AliceError, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct VerifiedBackup {
    pub path: PathBuf,
    pub schema_version: i64,
    pub memories: u64,
    pub chunks: u64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RestoreReport {
    pub restored_from: String,
    /// File name in the backups directory, when there was a current database.
    pub saved_current_as: Option<String>,
    pub memories: u64,
    pub chunks: u64,
}

pub fn verify(path: &Path) -> Result<VerifiedBackup> {
    let not_ours = |why: String| AliceError::invalid(format!(
        "{} is not an Alice backup ({why}); pick one from `alice backup list`", path.display()
    ));
    let c = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|e| not_ours(e.to_string()))?;
    let problems = alice_core::store::maintenance::quick_check(&c).map_err(|e| not_ours(e.to_string()))?;
    if !problems.is_empty() {
        return Err(AliceError::invalid(format!(
            "{} is damaged ({}); pick another backup", path.display(), problems[0]
        )));
    }
    let version: Option<i64> = c
        .query_row("SELECT max(version) FROM schema_migrations", [], |r| r.get(0))
        .optional()
        .map_err(|e| not_ours(e.to_string()))?
        .flatten();
    let version = version.ok_or_else(|| not_ours("no schema version".into()))?;
    if version > alice_core::store::migrations::CURRENT_VERSION {
        return Err(AliceError::invalid(format!(
            "{} was made by a newer Alice (schema v{version}, this build reads up to v{}); upgrade Alice first",
            path.display(), alice_core::store::migrations::CURRENT_VERSION
        )));
    }
    let count = |t: &str| -> Result<u64> {
        Ok(c.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get::<_, i64>(0)).map_err(|e| not_ours(e.to_string()))? as u64)
    };
    Ok(VerifiedBackup { path: path.to_path_buf(), schema_version: version, memories: count("memories")?, chunks: count("chunks")? })
}

/// A backup by file name (in `dir`), by path, or the newest one.
pub fn find_backup(_home: &AliceHome, dir: &Path, name_or_path: Option<&str>) -> Result<PathBuf> {
    match name_or_path {
        None => crate::backup::list(dir)?
            .into_iter()
            .find(|b| matches!(b.kind, crate::backup::BackupKind::Scheduled | crate::backup::BackupKind::Manual))
            .map(|b| b.path)
            .ok_or_else(|| AliceError::NotFound(format!("backups in {}; make one with `alice backup now`", dir.display()))),
        Some(s) => {
            let p = PathBuf::from(s);
            if p.is_file() {
                return Ok(p);
            }
            let named = dir.join(s);
            if named.is_file() {
                return Ok(named);
            }
            Err(AliceError::NotFound(format!("backup {s}; see `alice backup list`")))
        }
    }
}

pub fn swap(home: &AliceHome, dir: &Path, backup: &Path, now: DateTime<Utc>) -> Result<RestoreReport> {
    let verified = verify(backup)?;
    let Some(lock) = crate::daemon::try_lock_home(home)? else {
        return Err(AliceError::Other("a daemon is still running for this data; run `alice daemon stop` first".into()));
    };
    std::fs::create_dir_all(dir)?;
    let db = home.db();
    let mut saved = None;
    if db.exists() {
        let name = format!("alice-before-restore-{}.db", now.format("%Y%m%dT%H%M%SZ"));
        let dest = dir.join(&name);
        std::fs::rename(&db, &dest)?;
        // Keep the WAL with its database, so the saved copy opens with everything committed.
        for suffix in ["-wal", "-shm"] {
            let side = PathBuf::from(format!("{}{suffix}", db.display()));
            if side.exists() {
                std::fs::rename(&side, PathBuf::from(format!("{}{suffix}", dest.display())))?;
            }
        }
        saved = Some(name);
    }
    let tmp = db.with_extension("db.restore.tmp");
    std::fs::copy(backup, &tmp)?;
    std::fs::File::open(&tmp)?.sync_all()?;
    std::fs::rename(&tmp, &db)?;
    if let Ok(rd) = std::fs::read_dir(home.root().join("vectors")) {
        for e in rd.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
    drop(lock);
    Ok(RestoreReport {
        restored_from: backup.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        saved_current_as: saved,
        memories: verified.memories,
        chunks: verified.chunks,
    })
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-daemon restore::` — Expected: 3 passed.

- [ ] **Step 4: `alice restore`**

`main.rs`:

```rust
    /// Replace the memory database with a backup (default: the newest). The current database is
    /// kept in backups/; the daemon restarts and rebuilds its index.
    Restore {
        /// A backup file name from `alice backup list`, or a path.
        backup: Option<String>,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
```

`cmd.rs` (dispatch `Command::Restore` outside `client_command`, since it must not auto-start a daemon first):

```rust
async fn restore(home: &AliceHome, backup: Option<String>, yes: bool, json: bool) -> Result<()> {
    use std::io::{BufRead, IsTerminal, Write};
    let settings = alice_daemon::config::load_effective(home).resolved.settings; // Task 5's rule
    let dir = alice_daemon::backup::backup_dir(home, &settings.backup);
    let path = alice_daemon::restore::find_backup(home, &dir, backup.as_deref())?;
    let v = alice_daemon::restore::verify(&path)?;
    if !yes {
        anyhow::ensure!(std::io::stdin().is_terminal(), "restoring replaces the memory database; pass --yes to confirm");
        eprint!("Restore {} ({} memories, {} chunks)? The current database is kept in {}. [y/N] ",
            path.display(), v.memories, v.chunks, dir.display());
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        anyhow::ensure!(matches!(answer.trim(), "y" | "Y" | "yes"), "restore cancelled");
    }
    // Quiet: `restore` prints one JSON document (its report), never stop's output too.
    // Waits until the daemon has let go of its lock (writes spool meanwhile).
    if let StopOutcome::Stopping(pid) = stop_daemon(home).await? {
        anyhow::bail!("the daemon{} is still shutting down; retry `alice restore` in a moment",
            pid.map(|p| format!(" (pid {p})")).unwrap_or_default());
    }
    let report = alice_daemon::restore::swap(home, &dir, &path, chrono::Utc::now())?;
    let c = client(home).await?; // starts the daemon on the restored database
    for _ in 0..600 {
        if c.health().await?.index_ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    print(json, &report, || format!(
        "Restored {}: {} memories, {} chunks.{}\n",
        report.restored_from, report.memories, report.chunks,
        report.saved_current_as.as_deref().map(|s| format!(" The previous database is backups/{s}.")).unwrap_or_default()
    ))
}
```

M1's `stop(home, json)` prints from three branches. Split it: move its loop into a non-printing

```rust
/// What stopping found; `stop` prints it, `restore` only acts on it.
pub(crate) enum StopOutcome {
    /// The daemon that ran (pid if known) has saved and let go of its lock.
    Stopped(Option<u32>),
    /// Asked to stop, still shutting down when the wait ended.
    Stopping(Option<u32>),
    NotRunning,
}

/// M1's `stop` loop, unchanged except that each `return print(..)`/`return stopped(..)`
/// becomes `return Ok(StopOutcome::…)` (the `bail!` for a lock holder that never answers stays).
async fn stop_daemon(home: &AliceHome) -> Result<StopOutcome>
```

and keep `stop(home, json)` printing exactly what M1 printed for each outcome (`{"stopped": true, "pid": …}` / "stopped", `{"stopped": false, "stopping": true, "pid": …}` / "stop requested; still shutting down…", `{"stopped": false, "running": false}` / "not running"), so `alice daemon stop` and its tests do not change.

- [ ] **Step 5: The round trip (process test, unix)**

```rust
#[test]
fn restore_round_trip_brings_back_the_backup_and_keeps_writes_made_meanwhile() {
    let env = Env::new();
    env.ok(&["remember", "alpha before the backup"]);
    let b = env.json(&["backup", "now"]);
    let name = b["name"].as_str().unwrap().to_string();
    env.ok(&["remember", "bravo after the backup"]);
    env.ok(&["daemon", "stop"]);
    // Written while the daemon is down (as during a restore): spooled, applied after it.
    let out = env.cmd(&["remember", "charlie during the restore"]).env("ALICE_NO_AUTOSTART", "1").output().unwrap();
    assert!(out.status.success());
    // `env.json` parses the whole stdout as one JSON value: stop's own output would break it.
    let r = env.json(&["restore", &name, "--yes"]);
    assert_eq!(r["restored_from"], name);
    eventually(|| env.recall_count("charlie") == 1, "the spooled write drained into the restored database");
    assert_eq!(env.recall_count("alpha"), 1);
    assert_eq!(env.recall_count("bravo"), 0, "written after the backup: gone with the restore");
    let list = env.json(&["backup", "list"]);
    assert!(list.as_array().unwrap().iter().any(|b| b["kind"] == "before_restore"));
    let status = env.json(&["status"]);
    assert_eq!(status["index_ready"], true);
    // A bad file is refused before anything changes.
    std::fs::write(env.home.join("backups/alice-20200101T000000Z.db"), b"garbage").unwrap();
    let bad = env.run(&["restore", "alice-20200101T000000Z.db", "--yes"]);
    assert!(!bad.status.success());
    assert_eq!(env.recall_count("alpha"), 1, "nothing was swapped");
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-memory --test cli restore_round_trip` — Expected: pass; then the full suite; `pgrep -fl "alice daemon run"` empty.

- [ ] **Step 6: Commit**

```bash
nice -n 15 cargo fmt --all && nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
git add crates
git commit -m "feat: alice restore with backup verification, a saved copy of the replaced database and a tested round trip

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 16: HTTP embedding providers and model-change re-embed

Spec §4.2: providers `local` (ONNX), `ollama`, `openai` (any OpenAI-compatible endpoint), `voyage`; "each vector records its model. Changing the model triggers a background re-embed into a new index; search keeps using the old index until the new one is complete, then switches atomically." §4.2 and §11 put the HTTP providers in M2 (they are what Intel macOS and musl Linux need, and the M1 ledger deferred the model-change re-queue to M2). This task adds one HTTP embedder for the three remote providers, an embedder factory driven by `[embedding]` settings, and a re-embed worker that fills the new model's vectors and index while the old model serves, pauses the main embedding worker for a final catch-up pass, swaps model, embedder and index together, and leaves the old vectors to a maintenance purge. A model change made while the daemon is stopped is picked up at start, and an interrupted re-embed resumes.

**Files:**
- Create: `crates/alice-embed/src/http.rs` (+ `pub mod http;`, `pub fn factory`, `pub fn expected_model_id`)
- Modify: `crates/alice-core/src/api.rs` (`ReembedStatus`, so `Health` can carry it), `crates/alice-core/src/embed.rs` (`FakeEmbedder::named`), `crates/alice-core/src/store/embeddings.rs` (`Owner`, `missing`, `put_model_if_current`, `delete_vector`, `delete_model`), `crates/alice-core/src/settings.rs` (embedding keys apply live)
- Create: `crates/alice-daemon/src/reembed.rs`
- Modify: `crates/alice-daemon/src/paths.rs` (`vector_file` sanitises model ids), `writer.rs` (`SetReembedTarget`, `StoreModelVectors`, `DeleteModelVectors`; stale target vectors dropped in `StoreEmbeddings`), `worker.rs` (reads the active embedder per batch; honours a pause flag), `service.rs` (`Active` slot, start-up model check, live switch, `Health.embedding_model`, `Health.reembed`), `maintenance.rs` (`Job::ModelPurge`), `daemon.rs` (factory from settings)
- Test: `http.rs` (mock server), `reembed.rs`/service tests

**Interfaces:**
- Consumes: Task 2 (`OnnxEmbedder`, `OnnxOptions`), Task 9 (`BlockingHttp`), Task 5/6 (`EmbeddingSettings`, `config::follow_settings`, `SecretName::EmbeddingApiKey`, `Service::test_provider`), Task 12 (`JudgeWorkerConfig.model` closure), Task 14 (`Job`, scheduler).
- Produces:
  - `alice_embed::http::{HttpEmbedder, HttpProvider::{OpenAi, Ollama, Voyage}}`; `HttpEmbedder::new(provider, model: &str, endpoint: Option<&str>, key: Option<String>, batch: usize) -> Result<Self>` (probes the dimension with one call); model id `"<provider>:<model>"` (`openai`, `ollama`, `voyage`).
  - `alice_embed::{EmbedderFactory = Arc<dyn Fn(&EmbeddingSettings, Option<String>) -> Result<Arc<dyn Embedder>> + Send + Sync>, factory(models_dir: PathBuf) -> EmbedderFactory, expected_model_id(&EmbeddingSettings) -> String}`
  - `alice_core::embed::FakeEmbedder::named(id: impl Into<String>, dim: usize) -> Self`
  - `alice_core::store::embeddings::{Owner::{Memory, Chunk}, missing(conn, owner, model, after_seq: i64, limit) -> Result<Vec<PendingItem>>, put_model_if_current(conn, key, stamp, model, v) -> Result<bool>, delete_vector(conn, key, model) -> Result<()>, delete_model(conn, model, limit) -> Result<usize>}`
  - `WriteOp::{SetReembedTarget(Option<String>), StoreModelVectors { model, items: Vec<(VecKey, String, Vec<f32>)> }, DeleteModelVectors { model, limit }}` (`StoreModelVectors` → `WriteOutcome::Embedded(applied)`)
  - `alice_core::api::ReembedStatus { model: String, done: u64, total: u64 }` (in `alice-core`, where `Health` lives: `alice-core` cannot name a daemon type); `alice_daemon::reembed::spawn_reembed(ReembedConfig) -> JoinHandle<()>`; meta keys `active_embedding` (settings JSON), `reembed.target` (settings JSON), `reembed.<model>.cursor.memory`, `reembed.<model>.cursor.chunk`, `purge_model`.
  - `ServiceOptions::embedder_factory: Option<EmbedderFactory>` (the daemon passes one; tests pass a fake one); `Service::{embedder(&self) -> Arc<dyn Embedder>, model(&self) -> String}`; `Health::{embedding_model: String, reembed: Option<ReembedStatus>, embedding_error: Option<String>}`; `Job::ModelPurge` ("model_purge", daily).
  - `AliceHome::vector_file(model)` → `vectors/<sanitised model>.usearch` (`:` and other characters outside `[A-Za-z0-9._-]` become `_`).

- [ ] **Step 1: Read the files this task modifies, as they are now.**

- [ ] **Step 2: The HTTP embedder, test first**

`crates/alice-embed/src/http.rs` tests (a mock OpenAI-compatible server, like `lib.rs`'s download test server, answering `POST …/embeddings`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Answers /v1/embeddings with 3-dimension vectors [len(text), 1, 0] (out of order, to check
    /// `index` is honoured); 401 for key "bad"; 429 for the first call when asked to.
    async fn mock(first_429: bool) -> (String, std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", l.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let (log, calls) = (log.clone(), calls.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        let n = s.read(&mut chunk).await.unwrap();
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf).to_string();
                        if let Some(i) = text.find("\r\n\r\n") {
                            let len: usize = text.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length: ").map(|v| v.trim().parse().unwrap())).unwrap_or(0);
                            if buf.len() >= i + 4 + len { break; }
                        }
                        if n == 0 { break; }
                    }
                    let text = String::from_utf8_lossy(&buf).to_string();
                    let (head, body) = text.split_once("\r\n\r\n").unwrap();
                    let req: serde_json::Value = serde_json::from_str(body).unwrap();
                    log.lock().unwrap().push(req.clone());
                    let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let reply = if head.to_ascii_lowercase().contains("bearer bad") {
                        (401, "{\"error\":\"bad key\"}".to_string())
                    } else if first_429 && n == 0 {
                        (429, "{}".to_string())
                    } else {
                        let inputs = req["input"].as_array().unwrap();
                        let data: Vec<serde_json::Value> = inputs.iter().enumerate().rev()
                            .map(|(i, t)| serde_json::json!({"index": i, "embedding": [t.as_str().unwrap().len() as f32, 1.0, 0.0]}))
                            .collect();
                        (200, serde_json::json!({"data": data}).to_string())
                    };
                    let resp = format!("HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nRetry-After: 0\r\nConnection: close\r\n\r\n{}", reply.0, reply.1.len(), reply.1);
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        (base, seen)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn embeds_in_order_normalised_with_a_probed_dimension() {
        let (base, seen) = mock(true).await;
        let e = tokio::task::spawn_blocking(move || {
            let e = HttpEmbedder::new(HttpProvider::Voyage, "voyage-3-lite", Some(&base), Some("k".into()), 2).unwrap();
            let v = e.embed_passages(&["a".into(), "bbb".into(), "cc".into()]).unwrap();
            let q = e.embed_query("dddd").unwrap();
            (e.model_id().to_string(), e.dim(), v, q)
        }).await.unwrap();
        assert_eq!((e.0.as_str(), e.1), ("voyage:voyage-3-lite", 3));
        let norm = |x: f32, y: f32| x / (x * x + y * y).sqrt();
        assert!((e.2[1][0] - norm(3.0, 1.0)).abs() < 1e-6, "order follows `index`: {:?}", e.2);
        assert!((e.3[0] - norm(4.0, 1.0)).abs() < 1e-6);
        let reqs = seen.lock().unwrap();
        assert!(reqs.iter().any(|r| r["input_type"] == "document"));
        assert!(reqs.iter().any(|r| r["input_type"] == "query"));
        assert!(reqs.iter().all(|r| r["input"].as_array().unwrap().len() <= 2), "batches of batch_size");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rejected_key_says_which_secret_to_set_and_ollama_needs_none() {
        let (base, _) = mock(false).await;
        let b2 = base.clone();
        let err = tokio::task::spawn_blocking(move || HttpEmbedder::new(HttpProvider::OpenAi, "m", Some(&b2), Some("bad".into()), 8).err().unwrap().to_string()).await.unwrap();
        assert!(err.contains("embedding_api_key"), "{err}");
        let ok = tokio::task::spawn_blocking(move || HttpEmbedder::new(HttpProvider::Ollama, "nomic-embed-text", Some(&base), None, 8).map(|e| e.model_id().to_string())).await.unwrap().unwrap();
        assert_eq!(ok, "ollama:nomic-embed-text");
        assert!(HttpEmbedder::new(HttpProvider::OpenAi, "m", Some("http://127.0.0.1:9"), None, 8).err().unwrap().to_string().contains("embedding_api_key"), "openai needs a key");
    }
}
```

Implement:

```rust
//! HTTP embedding providers (spec §4.2): OpenAI-compatible `/embeddings` (OpenAI, Ollama's
//! `/v1`, and other compatible servers) and Voyage (the same shape plus `input_type`). Vectors
//! are L2-normalised; the dimension is probed once at start.

use crate::net::{BlockingHttp, HttpError};
use alice_core::embed::Embedder;
use alice_core::error::{AliceError, Result};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpProvider {
    OpenAi,
    Ollama,
    Voyage,
}

impl HttpProvider {
    pub fn name(self) -> &'static str {
        match self {
            HttpProvider::OpenAi => "openai",
            HttpProvider::Ollama => "ollama",
            HttpProvider::Voyage => "voyage",
        }
    }
    fn default_endpoint(self) -> &'static str {
        match self {
            HttpProvider::OpenAi => "https://api.openai.com/v1",
            HttpProvider::Ollama => "http://127.0.0.1:11434/v1",
            HttpProvider::Voyage => "https://api.voyageai.com/v1",
        }
    }
    fn needs_key(self) -> bool {
        !matches!(self, HttpProvider::Ollama)
    }
}

pub struct HttpEmbedder {
    provider: HttpProvider,
    model: String,
    id: String,
    url: String,
    key: Option<String>,
    dim: usize,
    batch: usize,
    http: BlockingHttp,
}

const TIMEOUT: Duration = Duration::from_secs(60);
const RETRIES: u32 = 3;

impl HttpEmbedder {
    pub fn new(provider: HttpProvider, model: &str, endpoint: Option<&str>, key: Option<String>, batch: usize) -> Result<Self> {
        if provider.needs_key() && key.as_deref().is_none_or(|k| k.trim().is_empty()) {
            return Err(AliceError::invalid(format!(
                "the {} embedding provider needs an API key; set it with `alice config set-secret embedding_api_key`",
                provider.name()
            )));
        }
        let base = endpoint.unwrap_or(provider.default_endpoint()).trim_end_matches('/');
        let mut e = Self {
            provider,
            model: model.into(),
            id: format!("{}:{model}", provider.name()),
            url: format!("{base}/embeddings"),
            key,
            dim: 0,
            batch: batch.max(1),
            http: BlockingHttp::new(concat!("alice/", env!("CARGO_PKG_VERSION"))).map_err(AliceError::Io)?,
        };
        e.dim = e.call(&["dimension probe".to_string()], "query")?.pop().map(|v| v.len()).unwrap_or(0);
        if e.dim == 0 {
            return Err(AliceError::Other(format!("{} returned an empty vector; check embedding.model", e.id)));
        }
        Ok(e)
    }

    fn call(&self, inputs: &[String], input_type: &str) -> Result<Vec<Vec<f32>>> {
        let mut body = serde_json::json!({"model": self.model, "input": inputs});
        if self.provider == HttpProvider::Voyage {
            body["input_type"] = serde_json::Value::String(input_type.into());
        }
        let bytes = serde_json::to_vec(&body).expect("json");
        let mut last = String::new();
        for attempt in 0..=RETRIES {
            match self.http.post_json(&self.url, self.key.as_deref(), bytes.clone(), TIMEOUT) {
                Ok(r) if r.status == 200 => return self.parse(&r.body, inputs.len()),
                Ok(r) if r.status == 401 || r.status == 403 => {
                    return Err(AliceError::Other(format!(
                        "{} refused the API key ({}); set it with `alice config set-secret embedding_api_key`", self.id, r.status
                    )));
                }
                Ok(r) if r.status == 429 || r.status >= 500 => last = format!("{} answered {}", self.id, r.status),
                Ok(r) => {
                    let snippet: String = String::from_utf8_lossy(&r.body).chars().take(200).collect();
                    return Err(AliceError::Other(format!("{} answered {}: {snippet}; check embedding.model and embedding.endpoint", self.id, r.status)));
                }
                Err(HttpError::Timeout) => last = format!("{} did not answer in time", self.id),
                Err(HttpError::Connect(e) | HttpError::Other(e)) => last = format!("cannot reach {} at {} ({e})", self.id, self.url),
            }
            if attempt < RETRIES {
                std::thread::sleep(Duration::from_millis(500 * 2u64.pow(attempt)));
            }
        }
        Err(AliceError::Other(format!("{last}; memories stay searchable by words and are embedded later")))
    }

    fn parse(&self, body: &[u8], expected: usize) -> Result<Vec<Vec<f32>>> {
        #[derive(serde::Deserialize)]
        struct Item {
            index: usize,
            embedding: Vec<f32>,
        }
        #[derive(serde::Deserialize)]
        struct Resp {
            data: Vec<Item>,
        }
        let mut r: Resp = serde_json::from_slice(body)
            .map_err(|e| AliceError::Other(format!("{} sent an answer that could not be read ({e})", self.id)))?;
        if r.data.len() != expected {
            return Err(AliceError::Other(format!("{} returned {} vectors for {expected} texts", self.id, r.data.len())));
        }
        r.data.sort_by_key(|i| i.index);
        Ok(r.data
            .into_iter()
            .map(|i| {
                let n = i.embedding.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
                i.embedding.into_iter().map(|x| x / n).collect()
            })
            .collect())
    }
}

impl Embedder for HttpEmbedder {
    fn model_id(&self) -> &str {
        &self.id
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for b in texts.chunks(self.batch) {
            out.extend(self.call(b, "document")?);
        }
        Ok(out)
    }
    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        self.call(&[text.to_string()], "query")?.pop().ok_or_else(|| AliceError::Other("no vector".into()))
    }
}
```

`lib.rs`:

```rust
pub type EmbedderFactory = std::sync::Arc<
    dyn Fn(&alice_core::settings::EmbeddingSettings, Option<String>) -> alice_core::Result<std::sync::Arc<dyn alice_core::embed::Embedder>> + Send + Sync,
>;

/// The model id the settings ask for (what vectors are stored under).
pub fn expected_model_id(s: &alice_core::settings::EmbeddingSettings) -> String {
    use alice_core::settings::EmbeddingProvider as P;
    match s.provider {
        P::Local => MODEL_ID.to_string(),
        P::Openai => format!("openai:{}", s.model),
        P::Ollama => format!("ollama:{}", s.model),
        P::Voyage => format!("voyage:{}", s.model),
    }
}

/// Builds embedders from `[embedding]` settings; `key` is `embedding_api_key`.
pub fn factory(models_dir: std::path::PathBuf) -> EmbedderFactory {
    std::sync::Arc::new(move |s, key| {
        use alice_core::settings::EmbeddingProvider as P;
        let http = |p| -> alice_core::Result<std::sync::Arc<dyn alice_core::embed::Embedder>> {
            Ok(std::sync::Arc::new(http::HttpEmbedder::new(p, &s.model, s.endpoint.as_deref(), key.clone(), s.batch_size)?))
        };
        match s.provider {
            #[cfg(feature = "local")]
            P::Local => {
                let idle = Some(s.idle_unload.0).filter(|d| !d.is_zero());
                Ok(std::sync::Arc::new(OnnxEmbedder::new(&models_dir, OnnxOptions { idle_unload: idle, intra_threads: None })?))
            }
            #[cfg(not(feature = "local"))]
            P::Local => Err(alice_core::AliceError::invalid(
                "this build has no local embedding model; set embedding.provider to openai, ollama or voyage",
            )),
            P::Openai => http(http::HttpProvider::OpenAi),
            P::Ollama => http(http::HttpProvider::Ollama),
            P::Voyage => http(http::HttpProvider::Voyage),
        }
    })
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-embed http::` — Expected: pass.

- [ ] **Step 3: Store helpers, test first**

`embeddings.rs` tests:

```rust
    #[test]
    fn missing_lists_rows_without_a_vector_for_the_model_after_a_cursor() {
        let d = tempfile::tempdir().unwrap();
        let c = crate::store::open_writer(&d.path().join("a.db")).unwrap();
        let p = crate::store::projects::get_by_id(&c, crate::store::projects::GLOBAL_ID).unwrap();
        let mut seqs = Vec::new();
        for t in ["one", "two", "three"] {
            let m = crate::store::memories::prepare(crate::store::memories::NewMemoryInput { content: t.into(), ..Default::default() }, &p, "h".into()).unwrap();
            if let crate::store::memories::InsertOutcome::New { seq, .. } = crate::store::memories::insert(&c, &m).unwrap() { seqs.push(seq); }
        }
        put(&c, VecKey::Memory(seqs[1]), "new", &[1.0]).unwrap();
        let got: Vec<VecKey> = missing(&c, Owner::Memory, "new", 0, 10).unwrap().into_iter().map(|i| i.key).collect();
        assert_eq!(got, vec![VecKey::Memory(seqs[0]), VecKey::Memory(seqs[2])]);
        assert_eq!(missing(&c, Owner::Memory, "new", seqs[0], 10).unwrap().len(), 1);
        let stamp: String = c.query_row("SELECT content_hash FROM memories WHERE seq = ?1", [seqs[0]], |r| r.get(0)).unwrap();
        assert!(!put_model_if_current(&c, VecKey::Memory(seqs[0]), "stale", "new", &[0.5]).unwrap());
        assert!(put_model_if_current(&c, VecKey::Memory(seqs[0]), &stamp, "new", &[0.5]).unwrap());
        let state: String = c.query_row("SELECT embed_state FROM memories WHERE seq = ?1", [seqs[0]], |r| r.get(0)).unwrap();
        assert_eq!(state, "pending", "embed_state belongs to the active model and is untouched");
        delete_vector(&c, VecKey::Memory(seqs[0]), "new").unwrap();
        assert_eq!(count(&c, "new").unwrap(), 1);
        assert_eq!(delete_model(&c, "new", 10).unwrap(), 1);
        assert_eq!(count(&c, "new").unwrap(), 0);
    }
```

Implement:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Memory,
    Chunk,
}

/// Rows after `after_seq` with no vector for `model`, in seq order (live memories / chunks).
pub fn missing(conn: &Connection, owner: Owner, model: &str, after_seq: i64, limit: usize) -> Result<Vec<PendingItem>> {
    let sql = match owner {
        Owner::Memory => "SELECT m.seq, m.title, m.body, m.content_hash FROM memories m
            WHERE m.seq > ?1 AND m.status <> 'trashed'
              AND NOT EXISTS (SELECT 1 FROM embeddings e WHERE e.owner_type = 'memory' AND e.owner_seq = m.seq AND e.model = ?2)
            ORDER BY m.seq LIMIT ?3",
        Owner::Chunk => "SELECT c.seq, c.text, '', c.content_hash FROM chunks c
            WHERE c.seq > ?1
              AND NOT EXISTS (SELECT 1 FROM embeddings e WHERE e.owner_type = 'chunk' AND e.owner_seq = c.seq AND e.model = ?2)
            ORDER BY c.seq LIMIT ?3",
    };
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(params![after_seq, model, limit as i64], |r| {
        let seq: i64 = r.get(0)?;
        Ok(match owner {
            Owner::Memory => PendingItem { key: VecKey::Memory(seq), text: memory_embed_text(&r.get::<_, String>(1)?, &r.get::<_, String>(2)?), stamp: r.get(3)? },
            Owner::Chunk => PendingItem { key: VecKey::Chunk(seq), text: r.get(1)?, stamp: r.get(3)? },
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Store `model`'s vector if the row still holds the text stamped `stamp`; `embed_state` (which
/// belongs to the active model) is not touched.
pub fn put_model_if_current(conn: &Connection, key: VecKey, stamp: &str, model: &str, v: &[f32]) -> Result<bool> {
    let (sql, seq) = match key {
        VecKey::Memory(s) => ("SELECT EXISTS(SELECT 1 FROM memories WHERE seq = ?1 AND content_hash = ?2)", s),
        VecKey::Chunk(s) => ("SELECT EXISTS(SELECT 1 FROM chunks WHERE seq = ?1 AND content_hash = ?2)", s),
    };
    let current: bool = conn.query_row(sql, params![seq, stamp], |r| r.get(0))?;
    if current {
        put(conn, key, model, v)?;
    }
    Ok(current)
}

pub fn delete_vector(conn: &Connection, key: VecKey, model: &str) -> Result<()> {
    let (t, s) = key.parts();
    conn.execute("DELETE FROM embeddings WHERE owner_type = ?1 AND owner_seq = ?2 AND model = ?3", params![t, s, model])?;
    Ok(())
}

pub fn delete_model(conn: &Connection, model: &str, limit: usize) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM embeddings WHERE rowid IN (SELECT rowid FROM embeddings WHERE model = ?1 LIMIT ?2)",
        params![model, limit as i64],
    )?)
}
```

`embed.rs`: `FakeEmbedder { dim, id: String }`, `new(dim)` = `named("fake", dim)`, `model_id()` returns `&self.id`. `paths.rs`:

```rust
    /// Base path of the vector index for `model` (characters a file name cannot hold become `_`).
    pub fn vector_file(&self, model: &str) -> PathBuf {
        let safe: String = model.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
        self.root.join("vectors").join(format!("{safe}.usearch"))
    }
```

Run: `nice -n 15 cargo test -j 6 -p alice-core embeddings::` — Expected: pass.

- [ ] **Step 4: Writer ops and the swappable active model**

`writer.rs`: `State` gains `reembed_target: Option<String>`. Ops:

```rust
        WriteOp::SetReembedTarget(t) => { st.reembed_target = t; WriteOutcome::Done }
        WriteOp::StoreModelVectors { model, items } => {
            let mut applied = Vec::new();
            for (key, stamp, v) in items {
                if embeddings::put_model_if_current(conn, key, &stamp, &model, &v)? {
                    applied.push((key, v));
                }
            }
            WriteOutcome::Embedded(applied)
        }
        WriteOp::DeleteModelVectors { model, limit } => WriteOutcome::Count(embeddings::delete_model(conn, &model, limit)?),
```

and in `StoreEmbeddings`, for each applied key, when `st.reembed_target` is `Some(t)` and `t != model`: `embeddings::delete_vector(conn, key, t)?` (the text changed; the re-embed's final pass embeds it again).

`service.rs`:

```rust
/// The embedding model in use: queries, the embedding worker and the judge read it per use;
/// a model switch replaces all three parts at once.
pub struct Active {
    pub embedder: Arc<dyn Embedder>,
    pub model: String,
    pub vector_path: std::path::PathBuf,
}

pub type ActiveSlot = RwLock<Arc<Active>>;
```

`Service` keeps `active: Arc<ActiveSlot>` (replacing the `embedder` field; `embedder()` and `model()` read it; **cross-task edit:** Task 6's `test_provider("embedding")` reads `self.embedder.clone()` and `self.embedder.model_id()` — change both to `self.embedder()`, and grep `service.rs` for any other `self.embedder` field use), and `WorkerConfig` takes `active: Arc<ActiveSlot>` instead of `embedder`/`vector_path`, reading `cfg.active.read().clone()` at the top of each loop, plus `paused: Arc<AtomicBool>` (when set, the worker naps and embeds nothing). The judge worker's `model` closure reads `active`. `recall` takes the embedder from `active` per request. Task 14's `MaintenanceCtx` replaces its `model` closure and `dim` with `active: Arc<ActiveSlot>` (the index check and the merge job read the current model and dimension from it).

- [ ] **Step 5: The re-embed worker, test first**

Service test (with a factory that builds named fakes):

```rust
    fn fake_factory() -> alice_embed::EmbedderFactory {
        Arc::new(|s: &alice_core::settings::EmbeddingSettings, _key| {
            let id = alice_embed::expected_model_id(s);
            let dim = if id == alice_embed::MODEL_ID { 384 } else { 64 };
            Ok(Arc::new(FakeEmbedder::named(id, dim)) as Arc<dyn Embedder>)
        })
    }

    async fn svc_switchable() -> (tempfile::TempDir, Arc<Service>) {
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::named(alice_embed::MODEL_ID, 384)) as Arc<dyn Embedder>),
            ServiceOptions { embedder_factory: Some(fake_factory()), ..Default::default() },
        ).unwrap();
        (d, s)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_model_change_re_embeds_in_the_background_then_switches_atomically() {
        let (d, s) = svc_switchable().await;
        let cwd = repo(&d, "proj");
        for i in 0..40 {
            s.remember(RememberRequest { content: format!("memory number {i} about deployment"), cwd: cwd.clone(), ..Default::default() }).await.unwrap();
        }
        s.ingest(IngestRequest { source: SourceInput { uri: "s".into(), ..Default::default() },
            chunks: (0..60).map(|i| ChunkInput { ord: i, text: format!("chunk {i} of a session about lease timeouts"), ..Default::default() }).collect(),
            cwd: cwd.clone(), ..Default::default() }).await.unwrap();
        wait_settled(&s).await;
        let mut m = serde_json::Map::new();
        m.insert("embedding.provider".into(), serde_json::json!("ollama"));
        m.insert("embedding.model".into(), serde_json::json!("nomic-embed-text"));
        let out = s.patch_settings(m).unwrap();
        assert!(out.restart_required.is_empty(), "applies live");
        // Until the switch, the old model answers queries.
        let mut saw_progress = false;
        for _ in 0..500 {
            let h = s.health().await.unwrap();
            if h.embedding_model == "ollama:nomic-embed-text" && h.reembed.is_none() {
                break;
            }
            if let Some(r) = &h.reembed {
                saw_progress |= r.total > 0;
                assert_eq!(h.embedding_model, alice_embed::MODEL_ID);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let h = s.health().await.unwrap();
        assert_eq!(h.embedding_model, "ollama:nomic-embed-text");
        assert!(saw_progress || h.reembed.is_none());
        // Every memory and chunk has a vector for the new model; search uses it.
        let new_count: i64 = s.read(|c| Ok(c.query_row("SELECT count(*) FROM embeddings WHERE model = 'ollama:nomic-embed-text'", [], |r| r.get(0))?)).await.unwrap();
        assert_eq!(new_count, 100);
        let found = s.recall(RecallRequest { query: "memory number 7 about deployment".into(), cwd: cwd.clone(), format: OutputFormat::Json, ..Default::default() }).await.unwrap();
        assert!(found.results[0].matched.contains(&MatchVia::Vector));
        // The old vectors go with the daily purge.
        s.run_job(crate::maintenance::Job::ModelPurge).await.unwrap();
        let old: i64 = s.read(|c| Ok(c.query_row("SELECT count(*) FROM embeddings WHERE model = ?1", [alice_embed::MODEL_ID], |r| r.get(0))?)).await.unwrap();
        assert_eq!(old, 0);
        assert!(!s.home().vector_file(alice_embed::MODEL_ID).with_extension("chunks.bq").exists());
    }

    /// A target embedder that counts the texts it embeds and takes `delay` per batch.
    struct Counting {
        inner: FakeEmbedder,
        texts: Arc<std::sync::atomic::AtomicUsize>,
        delay: std::time::Duration,
    }

    impl Embedder for Counting {
        fn model_id(&self) -> &str { self.inner.model_id() }
        fn dim(&self) -> usize { self.inner.dim() }
        fn embed_passages(&self, t: &[String]) -> alice_core::Result<Vec<Vec<f32>>> {
            std::thread::sleep(self.delay);
            self.texts.fetch_add(t.len(), std::sync::atomic::Ordering::SeqCst);
            self.inner.embed_passages(t)
        }
        fn embed_query(&self, q: &str) -> alice_core::Result<Vec<f32>> { self.inner.embed_query(q) }
        // Forward any other trait method (Task 2's `is_loaded`, …) to `inner` the same way.
    }

    /// The local model as before; any other model is a `Counting` fake.
    fn counting_factory(texts: Arc<std::sync::atomic::AtomicUsize>, delay: std::time::Duration) -> alice_embed::EmbedderFactory {
        Arc::new(move |s: &alice_core::settings::EmbeddingSettings, _key| {
            let id = alice_embed::expected_model_id(s);
            if id == alice_embed::MODEL_ID {
                return Ok(Arc::new(FakeEmbedder::named(id, 384)) as Arc<dyn Embedder>);
            }
            Ok(Arc::new(Counting { inner: FakeEmbedder::named(id, 64), texts: texts.clone(), delay }) as Arc<dyn Embedder>)
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_model_changed_while_stopped_is_picked_up_at_start_and_resumes() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        use std::time::Duration;
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let start = |home: AliceHome, f: alice_embed::EmbedderFactory| Service::start(home,
            &|| Ok(Arc::new(FakeEmbedder::named(alice_embed::MODEL_ID, 384)) as Arc<dyn Embedder>),
            ServiceOptions { embedder_factory: Some(f), ..Default::default() }).unwrap();
        let s = start(home.clone(), counting_factory(Arc::new(AtomicUsize::new(0)), Duration::ZERO));
        for i in 0..200 {
            s.remember(RememberRequest { content: format!("note {i}"), ..Default::default() }).await.unwrap();
        }
        wait_settled(&s).await;
        s.shutdown();
        drop(s);
        std::fs::write(home.config_file(), "[embedding]\nprovider = \"voyage\"\nmodel = \"voyage-3-lite\"\n").unwrap();
        // A slow target: stop the service once some progress is saved, part-way through.
        let first = Arc::new(AtomicUsize::new(0));
        let s = start(home.clone(), counting_factory(first.clone(), Duration::from_millis(100)));
        let key = "reembed.voyage:voyage-3-lite.cursor.memory";
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let cursor: Option<String> = s.read(move |c| alice_core::store::maintenance::get_meta(c, key)).await.unwrap();
            if cursor.and_then(|v| v.parse::<i64>().ok()).is_some_and(|c| c > 0) { break; }
            assert!(std::time::Instant::now() < deadline, "no re-embed progress was saved");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        s.shutdown(); // sets the re-embed's stop flag and joins its thread
        drop(s);
        let before = first.load(SeqCst);
        assert!(before > 0 && before < 200, "stopped part-way: {before} of 200");
        // Restart: it resumes and only embeds what is left, then switches.
        let second = Arc::new(AtomicUsize::new(0));
        let s = start(home.clone(), counting_factory(second.clone(), Duration::ZERO));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while s.health().await.unwrap().embedding_model != "voyage:voyage-3-lite" {
            assert!(std::time::Instant::now() < deadline, "the resumed re-embed did not switch");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let n: i64 = s.read(|c| Ok(c.query_row("SELECT count(*) FROM embeddings WHERE model = 'voyage:voyage-3-lite'", [], |r| r.get(0))?)).await.unwrap();
        assert_eq!(n, 200, "every memory has a vector for the new model");
        let after = second.load(SeqCst);
        assert!(after < 200 && before + after >= 200, "resumed, not restarted: {before} before the stop, {after} after");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_re_embed_that_cannot_record_its_progress_stops_and_says_why() {
        let (d, s) = svc_switchable().await;
        s.remember(RememberRequest { content: "one memory".into(), cwd: repo(&d, "proj"), ..Default::default() }).await.unwrap();
        wait_settled(&s).await;
        s.force_read_only("the disk is full");
        let mut m = serde_json::Map::new();
        m.insert("embedding.provider".into(), serde_json::json!("ollama"));
        m.insert("embedding.model".into(), serde_json::json!("nomic-embed-text"));
        s.patch_settings(m).unwrap(); // config.toml is a file, not the database: this still works
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let h = loop {
            let h = s.health().await.unwrap();
            if h.embedding_error.is_some() { break h; }
            assert!(std::time::Instant::now() < deadline, "the failure never reached health");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert!(h.embedding_error.unwrap().contains("re-embedding stopped"));
        assert_eq!(h.embedding_model, alice_embed::MODEL_ID, "the old model stays active");
    }
```

`Service` keeps the running re-embed's `(stop flag, JoinHandle)` in a mutex; `shutdown` sets the flag and joins the thread before joining the writer (a stop mid-batch saves the index and the cursor, so the next start resumes). (Start-up rule: with a factory, the *configured* embedder is `factory(settings.embedding)` and `make_embedder` is not used; without one (most tests), `make_embedder` is the configured embedder and no switching happens. In the second test, at start the service finds `meta.active_embedding` = local while the configured settings say voyage, so it builds the local one with the factory as the active embedder and re-embeds into voyage.)

Implement `crates/alice-daemon/src/reembed.rs`:

```rust
//! Re-embedding after a model change (spec §4.2): the new model's vectors and index are filled
//! in the background while the old model serves queries; then the main embedding worker pauses,
//! a final pass catches rows written or edited meanwhile, and model, embedder and index are
//! swapped at once. Progress is in `meta`, so a restart resumes where it stopped.

use crate::service::{Active, ActiveSlot, IndexSlot};
use crate::writer::{WriteOp, WriteOutcome, WriterHandle};
pub use alice_core::api::ReembedStatus; // defined in api.rs next to `Health`:
// #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
// pub struct ReembedStatus { pub model: String, pub done: u64, pub total: u64 }
use alice_core::embed::Embedder;
use alice_core::store::ReaderPool;
use alice_core::store::embeddings::{self, Owner};
use alice_core::vector::VectorIndex;
use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

pub struct Progress {
    pub model: String,
    pub done: AtomicU64,
    pub total: AtomicU64,
}

pub struct ReembedConfig {
    pub readers: Arc<ReaderPool>,
    pub writer: WriterHandle,
    pub target: Arc<dyn Embedder>,
    /// Settings JSON of the target, written to `meta.active_embedding` at the switch.
    pub target_settings: String,
    pub vector_path: PathBuf,
    pub active: Arc<ActiveSlot>,
    pub index: Arc<IndexSlot>,
    pub main_paused: Arc<AtomicBool>,
    pub stop: Arc<AtomicBool>,
    pub progress: Arc<Progress>,
    pub batch: usize,
    /// Set (with the reason) when the target model keeps failing.
    pub error: Arc<RwLock<Option<String>>>,
}

fn meta(readers: &ReaderPool, key: &str) -> Option<String> {
    readers.with(|c| alice_core::store::maintenance::get_meta(c, key)).ok().flatten()
}

fn set_meta(w: &WriterHandle, key: &str, value: &str) -> alice_core::Result<()> {
    w.submit_blocking(WriteOp::SetMeta { key: key.into(), value: value.into() }).map(|_| ())
}

/// Stop this re-embed and say why in health (`embedding_error`); the old model stays active.
fn give_up(cfg: &ReembedConfig, what: &str, e: impl std::fmt::Display) {
    let msg = format!("re-embedding stopped: {what} failed ({e}); the current model stays active (alice doctor)");
    tracing::warn!("{msg}");
    *cfg.error.write() = Some(msg);
    cfg.main_paused.store(false, Ordering::SeqCst);
}

pub fn spawn_reembed(cfg: ReembedConfig) -> JoinHandle<()> {
    std::thread::Builder::new().name("alice-reembed".into()).spawn(move || run(cfg)).expect("spawn re-embed thread")
}

/// Embed rows missing a target vector after `cursor` (per owner); returns how many were done.
fn pass(cfg: &ReembedConfig, index: &VectorIndex, model: &str, owner: Owner, resume: bool) -> alice_core::Result<u64> {
    let key = format!("reembed.{model}.cursor.{}", if owner == Owner::Memory { "memory" } else { "chunk" });
    let mut cursor: i64 = if resume { meta(&cfg.readers, &key).and_then(|v| v.parse().ok()).unwrap_or(0) } else { 0 };
    let mut done = 0;
    let mut failures = 0u32;
    while !cfg.stop.load(Ordering::Relaxed) {
        let items = cfg.readers.with(|c| embeddings::missing(c, owner, model, cursor, cfg.batch))?;
        let Some(last) = items.last().map(|i| match i.key { embeddings::VecKey::Memory(s) | embeddings::VecKey::Chunk(s) => s }) else { break };
        let texts: Vec<String> = items.iter().map(|i| i.text.clone()).collect();
        match cfg.target.embed_passages(&texts) {
            Ok(vectors) if vectors.len() == items.len() => {
                failures = 0;
                let batch = items.into_iter().zip(vectors).map(|(i, v)| (i.key, i.stamp, v)).collect();
                if let WriteOutcome::Embedded(applied) = cfg.writer.submit_blocking(WriteOp::StoreModelVectors { model: model.into(), items: batch })? {
                    index.upsert_batch(&applied)?;
                    done += applied.len() as u64;
                    cfg.progress.done.fetch_add(applied.len() as u64, Ordering::Relaxed);
                }
                cursor = last;
                if resume {
                    set_meta(&cfg.writer, &key, &cursor.to_string())?;
                }
            }
            Ok(_) | Err(_) => {
                failures += 1;
                if failures >= 5 {
                    let msg = format!("re-embedding with {model} keeps failing; check the provider (alice config test embedding)");
                    *cfg.error.write() = Some(msg.clone());
                    return Err(alice_core::AliceError::Other(msg));
                }
                std::thread::sleep(Duration::from_secs(2u64.pow(failures)));
            }
        }
    }
    Ok(done)
}

fn run(cfg: ReembedConfig) {
    let model = cfg.target.model_id().to_string();
    if let Err(e) = cfg.writer.submit_blocking(WriteOp::SetReembedTarget(Some(model.clone()))) {
        return give_up(&cfg, "recording the target model", e);
    }
    if let Err(e) = set_meta(&cfg.writer, "reembed.target", &cfg.target_settings) {
        return give_up(&cfg, "recording the target settings", e);
    }
    let total = cfg.readers.with(|c| Ok(
        c.query_row("SELECT (SELECT count(*) FROM memories WHERE status <> 'trashed') + (SELECT count(*) FROM chunks)", [], |r| r.get::<_, i64>(0))? as u64
    )).unwrap_or(0);
    cfg.progress.total.store(total, Ordering::Relaxed);
    let index = match VectorIndex::open(&cfg.vector_path, &model, cfg.target.dim())
        .or_else(|_| cfg.readers.with(|c| VectorIndex::rebuild_from(c, &model, cfg.target.dim())))
    {
        Ok(i) => Arc::new(i),
        Err(e) => {
            *cfg.error.write() = Some(format!("cannot prepare the {model} index: {e}"));
            return;
        }
    };
    // On a stop or failure, keep what was done: the index file must match the stored
    // vectors, or a resumed run would skip rows (they have vectors) that the index lacks.
    let stop_here = |cfg: &ReembedConfig, index: &VectorIndex, failed: Option<alice_core::AliceError>| {
        if let Err(e) = index.save(&cfg.vector_path) {
            return give_up(cfg, "saving the new index", e);
        }
        match failed {
            Some(e) => give_up(cfg, "embedding", e), // `pass` also set its own message
            None => cfg.main_paused.store(false, Ordering::SeqCst),
        }
    };
    // Phase 1: everything, resumable.
    for owner in [Owner::Memory, Owner::Chunk] {
        match pass(&cfg, &index, &model, owner, true) {
            Err(e) => return stop_here(&cfg, &index, Some(e)),
            Ok(_) if cfg.stop.load(Ordering::Relaxed) => return stop_here(&cfg, &index, None),
            Ok(_) => {}
        }
    }
    // Phase 2: pause the main worker, catch up with rows written or edited meanwhile, switch.
    cfg.main_paused.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(200)); // the worker finishes its current batch
    for owner in [Owner::Memory, Owner::Chunk] {
        match pass(&cfg, &index, &model, owner, false) {
            Err(e) => return stop_here(&cfg, &index, Some(e)),
            Ok(_) if cfg.stop.load(Ordering::Relaxed) => return stop_here(&cfg, &index, None),
            Ok(_) => {}
        }
    }
    // Persist first, then switch in memory: if any step fails, the old model stays active and
    // health says why (a restart then resumes from the saved state).
    if let Err(e) = index.save(&cfg.vector_path) {
        return give_up(&cfg, "saving the new index", e);
    }
    if let Err(e) = set_meta(&cfg.writer, "active_embedding", &cfg.target_settings)
        .and_then(|_| set_meta(&cfg.writer, "reembed.target", ""))
        .and_then(|_| cfg.writer.submit_blocking(WriteOp::SetReembedTarget(None)).map(|_| ()))
    {
        return give_up(&cfg, "recording the switch", e);
    }
    let old = cfg.active.read().model.clone();
    *cfg.active.write() = Arc::new(Active { embedder: cfg.target.clone(), model: model.clone(), vector_path: cfg.vector_path.clone() });
    *cfg.index.write() = Some(index);
    cfg.main_paused.store(false, Ordering::SeqCst);
    if old != model
        && let Err(e) = set_meta(&cfg.writer, "purge_model", &old)
    {
        // The switch stands; only the clean-up of the old vectors waits for the next switch.
        *cfg.error.write() = Some(format!("switched to {model}, but the old vectors of {old} were not queued for purging ({e})"));
    }
    tracing::info!(from = %old, to = %model, "switched embedding model");
}
```

In `Service::start`: let `configured = settings.embedding`; the configured embedder is `factory(&configured, key)` when `opts.embedder_factory` is set (key from `SecretName::EmbeddingApiKey`), else `make_embedder()`. Read `meta.active_embedding` (settings JSON). If there is none, the configured embedder is active and its settings are written there. If its expected model id differs from the configured one and a factory exists: build the old one with the factory as the active embedder, open its index, and spawn the re-embed with the configured embedder as the target; if building the old one fails, make the configured one active and still run the re-embed into the active index (rows without vectors get them; search has partial vector coverage meanwhile). A non-empty `meta.reembed.target` equal to the configured settings means an interrupted re-embed: the same rule resumes it from its cursors. React to settings changes in Task 6's `follow_settings` loop (add to the `on_change` closure, or run a second `follow_settings` on its own subscription — either way it continues after a `Lagged` with the re-read diff and stops only on `Closed`): when a changed key starts with `embedding.` and the expected model id differs from the active one, stop a running re-embed for another target (its `stop` flag, then join its thread), build the target with the factory (on error set `embedding_error` and keep the current model), and spawn the re-embed. `Health` gains `embedding_model` (active), `reembed` (from `Progress` while running), `embedding_error`. Remove `embedding.provider`, `embedding.model`, `embedding.endpoint` from `RESTART_KEYS`.

`maintenance.rs`: `Job::ModelPurge` ("model_purge", daily): if `meta.purge_model` names a model, `DeleteModelVectors { model, limit: 500 }` per step until 0, then delete that model's index files (`vector_file(old)` with `.memories.usearch` and `.chunks.bq`) and clear the key.

`daemon.rs`: `let factory = alice_embed::factory(models.clone());` and `make` = `factory(&settings.get().embedding, key)` (fake embedder when `ALICE_EMBEDDER=fake`), passing `ServiceOptions { embedder_factory: Some(factory), … }` (none in fake mode).

Run: `nice -n 15 cargo test -j 6 --workspace` — Expected: pass.

- [ ] **Step 6: Commit**

```bash
nice -n 15 cargo fmt --all && nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
git add crates
git commit -m "feat: HTTP embedding providers (openai, ollama, voyage) and background re-embed with an atomic switch on model change

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 17: Health, `alice doctor`, `alice token rotate`, `alice service install`

Spec §12 M2 delivers "health"; §5.7 errors tell the agent to "Run alice doctor"; §8.3 lists `alice doctor`, `alice token rotate`, `alice service [install|uninstall]`; §2.2 rule 5: "`alice service install` registers a launchd agent (macOS), systemd user unit (Linux) or scheduled task (Windows)". This task completes `Health`, adds daemon-side and local checks behind `alice doctor`, token rotation that the running daemon applies at once, and service installation that is fully testable without touching the real launchd/systemd: plans are pure data, `--dry-run` prints them, `--target-dir` writes the files under a test directory and runs no command.

**Files:**
- Create: `crates/alice-daemon/src/doctor.rs`, `crates/alice-daemon/src/service_install.rs`; modify `lib.rs`
- Modify: `crates/alice-core/src/api.rs` (`Health` additions), `crates/alice-daemon/src/service.rs` (`health`), `http.rs` (`GET /api/v1/doctor`, `POST /api/v1/admin/token/rotate`, the guard's token behind a lock), `discovery.rs` (`rotate_token`: a new token written privately and atomically), `client.rs` (`doctor`, `rotate_token`), `crates/alice/src/{main.rs,cmd.rs}` (`doctor`, `token rotate`, `service install|uninstall|status`, richer `status`)
- Test: unit tests in the new modules, `tests/http_api.rs`, `crates/alice/tests/cli.rs`

**Interfaces:**
- Consumes: everything that reports state: Tasks 6 (settings errors, restart_pending), 9 (`JevStats`, judge warning), 11 (reviews open), 12 (judge backlog), 14 (maintenance meta, backups), 16 (embedding model, re-embed).
- Produces:
  - `Health` adds `reviews_open: u64`, `model_loaded: bool`, `spool_pending: u64`, `last_backup: Option<String>`, `settings_errors: u64`, `memory_bytes: Option<u64>` (RSS on Linux, physical footprint on macOS).
  - `alice_daemon::doctor::{CheckStatus::{Ok, Warn, Fail}, Check { name, status, detail, fix: Option<String> }, DoctorReport { checks: Vec<Check> }, daemon_checks(svc: &Service) -> Result<Vec<Check>> (async), local_checks(home: &AliceHome, daemon: Option<&Health>) -> Vec<Check>, self_memory_bytes() -> Option<u64>, free_disk_bytes(path: &Path) -> Option<u64>}`
  - `alice_daemon::service_install::{Platform::{MacOs, Linux, Windows}, InstallPlan { files: Vec<(PathBuf, String)>, commands: Vec<Vec<String>>, notes: Vec<String> }, plan_install(platform, root: &Path, exe: &Path, home: &AliceHome, uid: u32) -> InstallPlan, plan_uninstall(platform, root: &Path, uid: u32) -> InstallPlan, installed(platform, root: &Path) -> Option<PathBuf>, Runner, SystemRunner, apply(plan, runner: Option<&dyn Runner>) -> Result<()>, uninstall(platform, root, uid, runner: Option<&dyn Runner>) -> Result<()>}` (`runner: None` writes or removes files only).
  - HTTP: `GET /api/v1/doctor` → `DoctorReport`; `POST /api/v1/admin/token/rotate` → `{"rotated": true}` (the new token is written to the token file, never returned).
  - CLI: `alice doctor`, `alice token rotate`, `alice service install [--dry-run] [--target-dir DIR]`, `alice service uninstall [--dry-run] [--target-dir DIR]`, `alice service status`.

- [ ] **Step 1: Read the files this task modifies, as they are now.**

- [ ] **Step 2: Service install plans, test first**

`crates/alice-daemon/src/service_install.rs` tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> AliceHome {
        AliceHome::new(PathBuf::from("/Users/u/.alice"))
    }

    #[test]
    fn the_macos_plan_is_a_launch_agent_for_this_binary_and_home() {
        let root = PathBuf::from("/tmp/root");
        let p = plan_install(Platform::MacOs, &root, Path::new("/opt/alice/bin/alice"), &home(), 501);
        let (path, plist) = &p.files[0];
        assert_eq!(path, &root.join("Library/LaunchAgents/dev.alice.daemon.plist"));
        for want in ["<string>dev.alice.daemon</string>", "<string>/opt/alice/bin/alice</string>", "<string>daemon</string>",
                     "<string>run</string>", "<key>ALICE_HOME</key>", "<string>/Users/u/.alice</string>", "/Users/u/.alice/logs/daemon.log"] {
            assert!(plist.contains(want), "{want}\n{plist}");
        }
        assert_eq!(p.commands, vec![vec!["launchctl".to_string(), "bootstrap".into(), "gui/501".into(), path.display().to_string()]]);
        let u = plan_uninstall(Platform::MacOs, &root, 501);
        assert_eq!(u.commands[0], vec!["launchctl".to_string(), "bootout".into(), "gui/501/dev.alice.daemon".into()]);
    }

    #[test]
    fn the_linux_plan_is_a_systemd_user_unit_and_windows_a_logon_task() {
        let root = PathBuf::from("/tmp/root");
        let p = plan_install(Platform::Linux, &root, Path::new("/usr/local/bin/alice"), &home(), 1000);
        assert_eq!(p.files[0].0, root.join(".config/systemd/user/alice.service"));
        assert!(p.files[0].1.contains("ExecStart=/usr/local/bin/alice daemon run"));
        assert!(p.files[0].1.contains("Environment=ALICE_HOME=/Users/u/.alice"));
        assert_eq!(p.commands.last().unwrap(), &vec!["systemctl".to_string(), "--user".into(), "enable".into(), "--now".into(), "alice.service".into()]);
        assert!(p.notes.iter().any(|n| n.contains("loginctl enable-linger")));
        let w = plan_install(Platform::Windows, &root, Path::new(r"C:\alice\alice.exe"), &home(), 0);
        assert!(w.files.is_empty());
        assert_eq!(&w.commands[0][..3], &["schtasks".to_string(), "/Create".into(), "/SC".into()]);
        assert!(w.notes.iter().any(|n| n.contains("not yet verified")));
    }

    #[test]
    fn apply_writes_files_and_runs_commands_only_with_a_runner() {
        struct Rec(parking_lot::Mutex<Vec<Vec<String>>>);
        impl Runner for Rec {
            fn run(&self, cmd: &[String]) -> alice_core::Result<()> {
                self.0.lock().push(cmd.to_vec());
                Ok(())
            }
        }
        let d = tempfile::tempdir().unwrap();
        let p = plan_install(Platform::Linux, d.path(), Path::new("/x/alice"), &home(), 1000);
        apply(&p, None).unwrap();
        assert!(installed(Platform::Linux, d.path()).is_some());
        let rec = Rec(Default::default());
        uninstall(Platform::Linux, d.path(), 1000, Some(&rec)).unwrap();
        assert!(installed(Platform::Linux, d.path()).is_none());
        assert!(rec.0.lock().iter().any(|c| c.contains(&"disable".to_string())));
    }

    #[test]
    fn uninstall_ignores_not_loaded_but_returns_other_failures() {
        struct Fails(&'static str);
        impl Runner for Fails {
            fn run(&self, cmd: &[String]) -> alice_core::Result<()> {
                Err(AliceError::Other(format!("`{}` exited with exit status: 1: {}", cmd.join(" "), self.0)))
            }
        }
        let d = tempfile::tempdir().unwrap();
        let p = plan_install(Platform::Linux, d.path(), Path::new("/x/alice"), &home(), 1000);
        apply(&p, None).unwrap();
        let e = uninstall(Platform::Linux, d.path(), 1000, Some(&Fails("Failed to connect to bus: Permission denied"))).unwrap_err();
        assert!(e.to_string().contains("Permission denied"), "{e}");
        assert!(installed(Platform::Linux, d.path()).is_some(), "the unit file stays after a real failure");
        uninstall(Platform::Linux, d.path(), 1000, Some(&Fails("Unit alice.service not loaded."))).unwrap();
        assert!(installed(Platform::Linux, d.path()).is_none(), "not loaded is fine: the file goes");
    }
}
```

Implement:

```rust
//! `alice service install|uninstall` (spec §2.2 rule 5): a launchd agent on macOS, a systemd
//! user unit on Linux, a logon scheduled task on Windows. Plans are data, so they are tested
//! without touching the user's service manager; `apply` writes files and runs commands only
//! when given a runner.

use crate::paths::AliceHome;
use alice_core::error::{AliceError, Result};
use std::path::{Path, PathBuf};

pub const LABEL: &str = "dev.alice.daemon";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Linux,
    Windows,
}

impl Platform {
    pub fn current() -> Option<Platform> {
        if cfg!(target_os = "macos") {
            Some(Platform::MacOs)
        } else if cfg!(target_os = "linux") {
            Some(Platform::Linux)
        } else if cfg!(windows) {
            Some(Platform::Windows)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize)]
pub struct InstallPlan {
    pub files: Vec<(PathBuf, String)>,
    pub commands: Vec<Vec<String>>,
    pub notes: Vec<String>,
}

pub trait Runner {
    fn run(&self, cmd: &[String]) -> Result<()>;
}

pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, cmd: &[String]) -> Result<()> {
        let out = std::process::Command::new(&cmd[0]).args(&cmd[1..]).output()?;
        if !out.status.success() {
            // stderr carries the reason ("No such process", "Unit … not loaded", …); `uninstall`
            // tells "not installed" from real failures by it.
            let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(AliceError::Other(format!("`{}` exited with {}: {why}", cmd.join(" "), out.status)));
        }
        Ok(())
    }
}

/// Whether a failed stop/unregister command only means the service was not loaded.
fn not_installed(e: &AliceError) -> bool {
    let m = e.to_string().to_ascii_lowercase();
    ["no such process", "does not exist", "cannot find", "not loaded", "not found", "could not find"]
        .iter()
        .any(|p| m.contains(p))
}

fn unit_path(p: Platform, root: &Path) -> Option<PathBuf> {
    match p {
        Platform::MacOs => Some(root.join("Library/LaunchAgents").join(format!("{LABEL}.plist"))),
        Platform::Linux => Some(root.join(".config/systemd/user/alice.service")),
        Platform::Windows => None,
    }
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn plan_install(p: Platform, root: &Path, exe: &Path, home: &AliceHome, uid: u32) -> InstallPlan {
    let exe_s = exe.display().to_string();
    let home_s = home.root().display().to_string();
    let log = home.logs_dir().join("daemon.log").display().to_string();
    match p {
        Platform::MacOs => {
            let path = unit_path(p, root).expect("macOS has a plist");
            let plist = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>daemon</string><string>run</string></array>
  <key>EnvironmentVariables</key>
  <dict><key>ALICE_HOME</key><string>{}</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
                xml(&exe_s), xml(&home_s), xml(&log), xml(&log)
            );
            InstallPlan {
                commands: vec![vec!["launchctl".into(), "bootstrap".into(), format!("gui/{uid}"), path.display().to_string()]],
                files: vec![(path, plist)],
                notes: vec!["`alice daemon stop` stops it until the next login (it exits cleanly, so launchd does not restart it).".into()],
            }
        }
        Platform::Linux => {
            let path = unit_path(p, root).expect("Linux has a unit");
            let unit = format!(
                "[Unit]\nDescription=Alice memory daemon\n\n[Service]\nExecStart={exe_s} daemon run\nEnvironment=ALICE_HOME={home_s}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n"
            );
            InstallPlan {
                files: vec![(path, unit)],
                commands: vec![
                    vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
                    vec!["systemctl".into(), "--user".into(), "enable".into(), "--now".into(), "alice.service".into()],
                ],
                notes: vec!["User services stop at logout unless lingering is on: `loginctl enable-linger $USER`.".into()],
            }
        }
        Platform::Windows => InstallPlan {
            files: vec![],
            commands: vec![vec![
                "schtasks".into(), "/Create".into(), "/SC".into(), "ONLOGON".into(), "/TN".into(), "Alice".into(),
                "/TR".into(), format!("cmd /c set ALICE_HOME={home_s}&& \"{exe_s}\" daemon run"), "/RL".into(), "LIMITED".into(), "/F".into(),
            ]],
            notes: vec!["Windows support is not yet verified on a real machine; the scheduled task starts the daemon at logon.".into()],
        },
    }
}

pub fn plan_uninstall(p: Platform, root: &Path, uid: u32) -> InstallPlan {
    match p {
        Platform::MacOs => InstallPlan {
            commands: vec![vec!["launchctl".into(), "bootout".into(), format!("gui/{uid}/{LABEL}")]],
            files: vec![],
            notes: vec![],
        },
        Platform::Linux => InstallPlan {
            commands: vec![
                vec!["systemctl".into(), "--user".into(), "disable".into(), "--now".into(), "alice.service".into()],
                vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
            ],
            files: vec![],
            notes: vec![],
        },
        Platform::Windows => InstallPlan {
            commands: vec![vec!["schtasks".into(), "/Delete".into(), "/TN".into(), "Alice".into(), "/F".into()]],
            files: vec![],
            notes: vec![],
        },
    }
}

pub fn installed(p: Platform, root: &Path) -> Option<PathBuf> {
    unit_path(p, root).filter(|f| f.exists())
}

/// Install: write the files, then run the commands (when a runner is given).
pub fn apply(plan: &InstallPlan, runner: Option<&dyn Runner>) -> Result<()> {
    for (path, content) in &plan.files {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, content)?;
    }
    if let Some(r) = runner {
        for c in &plan.commands {
            r.run(c)?;
        }
    }
    Ok(())
}
```

```rust
/// Uninstall: stop and unregister (when a runner is given), then remove the unit file. A
/// service that is not loaded is not an error; any other command failure is returned (the unit
/// file stays, so `alice service status` still shows it and a retry can finish the job).
pub fn uninstall(p: Platform, root: &Path, uid: u32, runner: Option<&dyn Runner>) -> Result<()> {
    if let Some(r) = runner {
        for c in &plan_uninstall(p, root, uid).commands {
            match r.run(c) {
                Ok(()) => {}
                Err(e) if not_installed(&e) => tracing::info!(error = %e, "service was not loaded"),
                Err(e) => return Err(e),
            }
        }
    }
    if let Some(f) = installed(p, root) {
        std::fs::remove_file(f)?;
    }
    Ok(())
}
```

Run: `nice -n 15 cargo test -j 6 -p alice-daemon service_install::` — Expected: pass.

- [ ] **Step 3: Doctor checks, test first**

`crates/alice-daemon/src/doctor.rs` tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_checks_flag_loose_permissions_a_bad_config_and_a_missing_model() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().join("home"));
        home.ensure_dirs().unwrap();
        std::fs::write(home.token_file(), "t").unwrap();
        std::fs::write(home.config_file(), "[judge]\nreview_threshold = 2\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(home.token_file(), std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let checks = local_checks_with(&home, None, false);
        let get = |n: &str| checks.iter().find(|c| c.name == n).unwrap_or_else(|| panic!("no check {n}: {checks:?}"));
        assert_eq!(get("config").status, CheckStatus::Fail);
        assert!(get("config").detail.contains("judge.review_threshold"));
        #[cfg(unix)]
        assert_eq!(get("token permissions").status, CheckStatus::Fail);
        assert_eq!(get("embedding model").status, CheckStatus::Fail, "the local model is not downloaded here");
        assert!(get("embedding model").fix.as_deref().unwrap().contains("alice model fetch"));
        assert_eq!(get("daemon").status, CheckStatus::Warn);
        // With the test embedder (ALICE_EMBEDDER=fake) a missing model is only worth a warning.
        let fake = local_checks_with(&home, None, true);
        let model = fake.iter().find(|c| c.name == "embedding model").unwrap();
        assert_eq!(model.status, CheckStatus::Warn, "{model:?}");
    }

    #[test]
    fn memory_and_disk_probes_answer_on_this_platform() {
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(self_memory_bytes().unwrap() > 0);
            assert!(free_disk_bytes(Path::new(".")).unwrap() > 0);
        }
    }
}
```

Service test: `daemon_checks(&svc)` on a fresh service has no `Fail`; after `force_read_only("the disk is full")` the `database` check is `Fail` and its `fix` says what to do; with `judge.provider = jev` and no key, the `judge` check is `Warn` mentioning `set-secret jev_api_key`.

Implement (`doctor.rs`):

```rust
//! Health checks for `alice doctor` (spec §5.7): what is wrong and what to do next.

use crate::paths::AliceHome;
use crate::service::Service;
use alice_core::api::Health;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
    /// What to do about it.
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct DoctorReport {
    pub checks: Vec<Check>,
}

fn check(name: &str, status: CheckStatus, detail: impl Into<String>, fix: Option<&str>) -> Check {
    Check { name: name.into(), status, detail: detail.into(), fix: fix.map(String::from) }
}

/// The daemon's own memory: physical footprint on macOS, RSS on Linux.
pub fn self_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: proc_pid_rusage fills the zeroed struct for our own pid.
        let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::proc_pid_rusage(std::process::id() as libc::c_int, libc::RUSAGE_INFO_V4, (&mut info as *mut libc::rusage_info_v4).cast()) };
        return (rc == 0).then_some(info.ri_phys_footprint);
    }
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        // SAFETY: sysconf has no preconditions.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
        return Some(pages * page);
    }
    #[allow(unreachable_code)]
    None
}

pub fn free_disk_bytes(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: statvfs fills the zeroed struct for a valid C path.
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return None;
        }
        return Some(s.f_bavail as u64 * s.f_frsize as u64);
    }
    #[allow(unreachable_code)]
    {
        let _ = path;
        None
    }
}

#[cfg(unix)]
fn mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).ok().map(|m| m.permissions().mode() & 0o777)
}

/// Checks that need no daemon (and must work when it cannot start).
pub fn local_checks(home: &AliceHome, daemon: Option<&Health>) -> Vec<Check> {
    let fake_embedder = std::env::var("ALICE_EMBEDDER").is_ok_and(|v| v == "fake");
    local_checks_with(home, daemon, fake_embedder)
}

/// `local_checks` with the test-embedder switch passed in (tests set it either way).
pub(crate) fn local_checks_with(home: &AliceHome, daemon: Option<&Health>, fake_embedder: bool) -> Vec<Check> {
    use CheckStatus::*;
    let mut out = Vec::new();
    // Task 5's one rule for "config.toml + ALICE_* + defaults" (what the daemon would start with).
    let eff = crate::config::load_effective(home);
    if eff.errors.is_empty() {
        out.push(check("config", Ok, "config.toml and ALICE_* variables are valid", None));
    } else {
        let detail = eff.errors.iter().map(|e| format!("{}: {}", e.key, e.message)).collect::<Vec<_>>().join("; ");
        out.push(check("config", Fail, detail, Some("fix the keys named (alice config edit); the daemon keeps the last valid settings meanwhile")));
    }
    if !eff.resolved.warnings.is_empty() {
        out.push(check("environment", Warn, eff.resolved.warnings.join("; "), Some("unset or rename the variables named; `alice config list` shows the valid ones")));
    }
    let settings = eff.resolved.settings;
    #[cfg(unix)]
    {
        for (name, path, want) in [("home permissions", home.root().to_path_buf(), 0o700), ("token permissions", home.token_file(), 0o600), ("secrets permissions", home.secrets_file(), 0o600)] {
            match mode(&path) {
                None => {}
                Some(m) if m & !want == 0 => out.push(check(name, Ok, format!("{} is {:o}", path.display(), m), None)),
                Some(m) => out.push(check(name, Fail, format!("{} is {:o}; only you should be able to read it", path.display(), m),
                    Some(&format!("chmod {:o} {}", want, path.display())))),
            }
        }
    }
    if settings.embedding.provider == alice_core::settings::EmbeddingProvider::Local {
        if fake_embedder {
            out.push(check("embedding model", Warn, "ALICE_EMBEDDER=fake: a test embedder is in use and the local model is not checked",
                Some("unset ALICE_EMBEDDER to use the real model")));
        } else if alice_embed::is_model_present(&home.models_dir()) {
            out.push(check("embedding model", Ok, "multilingual-e5-small-q8 is downloaded", None));
        } else {
            out.push(check("embedding model", Fail, "the local embedding model is missing or incomplete", Some("run `alice model fetch`")));
        }
    }
    match daemon {
        Some(h) => {
            out.push(check("daemon", Ok, format!("running (pid {}, v{})", h.pid, h.version), None));
            if h.version != env!("CARGO_PKG_VERSION") {
                out.push(check("version", Warn, format!("the daemon is v{}, this CLI is v{}", h.version, env!("CARGO_PKG_VERSION")),
                    Some("run `alice daemon stop`; the next command starts the new version")));
            }
        }
        None => {
            let tail = std::fs::read_to_string(home.logs_dir().join("daemon.log")).unwrap_or_default();
            let last: Vec<&str> = tail.lines().rev().take(3).collect();
            out.push(check("daemon", Warn, format!("not running; last log lines: {}", last.into_iter().rev().collect::<Vec<_>>().join(" | ")),
                Some("start it with `alice daemon start`; if it exits at once, read `alice daemon logs`")));
        }
    }
    if let Some(p) = crate::service_install::Platform::current()
        && let Some(root) = dirs::home_dir()
    {
        match crate::service_install::installed(p, &root) {
            Some(f) => out.push(check("service", Ok, format!("installed ({})", f.display()), None)),
            None => out.push(check("service", Ok, "not installed (optional: the daemon starts on first use)", Some("`alice service install` keeps it running"))),
        }
    }
    out
}

/// Checks only the daemon can make (it alone opens the database).
pub async fn daemon_checks(svc: &Service) -> alice_core::Result<Vec<Check>> {
    use CheckStatus::*;
    let h = svc.health().await?;
    let view = svc.settings_view();
    let settings = svc.settings();
    let mut out = Vec::new();
    match &h.read_only {
        Some(r) => out.push(check("database", Fail, format!("read-only: {r}"), Some("free disk space or restore a backup (`alice restore`), then `alice daemon stop` and start again; writes are spooled meanwhile"))),
        None => out.push(check("database", Ok, format!("{} memories, {} chunks", h.memories, h.chunks), None)),
    }
    let status = svc.maintenance_status().await?;
    for j in &status {
        if let Some(e) = &j.last_error {
            out.push(check(&format!("job {}", j.job), Warn, e.clone(), Some(&format!("run `alice maintenance run {}` to retry", j.job))));
        }
    }
    out.push(match &h.last_backup {
        Some(t) => {
            let age = chrono::DateTime::parse_from_rfc3339(t).map(|t| chrono::Utc::now() - t.with_timezone(&chrono::Utc)).ok();
            match age {
                Some(a) if a > chrono::Duration::days(2) => check("backups", Warn, format!("the newest backup is {} days old", a.num_days()), Some("run `alice backup now`; check `alice maintenance status`")),
                _ => check("backups", Ok, format!("newest backup at {t}"), None),
            }
        }
        None => check("backups", Warn, "no backup yet", Some("run `alice backup now` (backups also run daily when the daemon is idle)")),
    });
    out.push(if h.index_ready { check("vector index", Ok, "ready", None) } else { check("vector index", Warn, "being rebuilt; search uses words only meanwhile", None) });
    let failed: u64 = svc.count("SELECT count(*) FROM memories WHERE embed_state = 'failed'").await?;
    if failed > 0 {
        out.push(check("embeddings", Warn, format!("{failed} memories have no vector and are found by words only"), Some("check `alice config test embedding`")));
    }
    if let Some(e) = &h.embedding_error {
        out.push(check("embedding provider", Fail, e.clone(), Some("fix [embedding] settings or the key, then `alice config test embedding`")));
    }
    out.push(match &h.judge_warning {
        Some(w) => check("judge", Warn, w.clone(), Some("`alice config set-secret jev_api_key`, or set judge.provider = \"heuristic\"")),
        None => check("judge", Ok, format!("{} (backlog {})", h.judge_provider, h.judge_backlog), None),
    });
    let stats = svc.stats().await?;
    if settings.judge.provider == alice_core::settings::JudgeProvider::Jev && stats.cost_today_usd >= 0.8 * settings.judge.daily_budget_usd {
        out.push(check("judge budget", Warn, format!("${:.4} of ${:.2} used today", stats.cost_today_usd, settings.judge.daily_budget_usd),
            Some("raise judge.daily_budget_usd if you expect more; at the limit the heuristic decides and Jev retries tomorrow")));
    }
    if h.reviews_open > 50 {
        out.push(check("reviews", Warn, format!("{} items wait for a decision", h.reviews_open), Some("`alice review`")));
    }
    if !view.errors.is_empty() {
        out.push(check("settings", Fail, view.errors.iter().map(|e| format!("{}: {}", e.key, e.message)).collect::<Vec<_>>().join("; "), Some("`alice config edit`")));
    }
    if !view.restart_pending.is_empty() {
        out.push(check("restart", Warn, format!("changed settings wait for a restart: {}", view.restart_pending.join(", ")), Some("`alice daemon stop`; the next command starts it again")));
    }
    if h.spool_pending > 0 {
        out.push(check("spool", Warn, format!("{} writes wait in the spool", h.spool_pending), Some("they are applied within seconds; if the number stays, read `alice daemon logs`")));
    }
    if let Some(b) = h.memory_bytes {
        let mb = b / 1_000_000;
        out.push(if mb > 800 { check("memory", Warn, format!("{mb} MB in use (budget 800 MB)"), Some("set embedding.idle_unload so the model unloads when idle")) } else { check("memory", Ok, format!("{mb} MB in use"), None) });
    }
    if let Some(free) = free_disk_bytes(svc.home().root()) {
        let gb = free as f64 / 1e9;
        out.push(match gb {
            g if g < 0.1 => check("disk", Fail, format!("{g:.2} GB free"), Some("free space; Alice goes read-only when the disk is full")),
            g if g < 1.0 => check("disk", Warn, format!("{g:.2} GB free"), Some("free some space")),
            g => check("disk", Ok, format!("{g:.1} GB free"), None),
        });
    }
    Ok(out)
}
```

(`Service::count(sql)` is a small `pub(crate)` helper reading one `i64` through `read`.) `service.rs` `health()` fills the new `Health` fields: `reviews_open` (`reviews::count_open`), `model_loaded` (`self.embedder().is_loaded()`), `spool_pending` (`spool::pending_count(&home.spool_dir())`), `last_backup` (newest `backup::list` entry's `created_at`), `settings_errors`, `memory_bytes` (`doctor::self_memory_bytes()`).

- [ ] **Step 4: Token rotation, test first**

`tests/http_api.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn rotating_the_token_takes_effect_at_once() {
    let s = Srv::start().await; // Task 4's helpers; `s.token` is M1's "t" x 64
    let old = s.token.clone();
    let r = s.post("/api/v1/admin/token/rotate", &serde_json::json!({})).await;
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    assert!(!body.contains(&old) && body.contains("rotated"));
    let new = std::fs::read_to_string(s.home.token_file()).unwrap().trim().to_string();
    assert_ne!(new, old);
    assert_eq!(s.http.get(s.url("/api/v1/health")).bearer_auth(&old).send().await.unwrap().status(), 401);
    assert_eq!(s.http.get(s.url("/api/v1/health")).bearer_auth(&new).send().await.unwrap().status(), 200);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(s.home.token_file()).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
```

Implement: `Guard.token` and `AppState.token` become `Arc<parking_lot::RwLock<Arc<str>>>` (the same `Arc` shared by both; read the current value per request and per ping proof). `discovery.rs`:

```rust
/// Replace the token with a new random one (written privately and atomically); returns it.
pub fn rotate_token(home: &AliceHome) -> std::io::Result<String> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| std::io::Error::other(e.to_string()))?;
    let token = hex::encode(buf);
    let tmp = home.root().join(format!("token.{}.tmp", new_id()));
    write_private(&tmp, token.as_bytes())?;
    std::fs::rename(&tmp, home.token_file())?;
    Ok(token)
}
```

Route (guarded): `POST /api/v1/admin/token/rotate` → `discovery::rotate_token(st.svc.home())` (in `spawn_blocking`), then `*st.token.write() = new.into()`, answer `{"rotated": true}`; record a `token.rotate` event (no value). Client: `rotate_token()`. CLI `alice token rotate`: with a running daemon, call the route; without one, `discovery::rotate_token` directly. Print "Token rotated. Clients that cached the old token reconnect on their next call." (`alice mcp` shims re-discover through the ping proof, which fails with the old token, and read the new file.)

- [ ] **Step 5: `alice doctor`, `alice status`, `alice service`**

`main.rs`:

```rust
    /// Check the installation, the daemon and the data, with what to do about each problem.
    Doctor,
    #[command(subcommand)]
    Token(cmd::TokenCmd),
    /// Run the daemon at login (launchd, systemd --user, or a scheduled task).
    #[command(subcommand)]
    Service(cmd::ServiceCmd),
```

`cmd.rs`:

```rust
#[derive(Subcommand)]
pub enum TokenCmd {
    /// Replace the API token; the daemon switches at once.
    Rotate,
}

#[derive(Subcommand)]
pub enum ServiceCmd {
    Install {
        /// Print the files and commands; change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Write the files under this directory instead of your home and run no command (for tests).
        #[arg(long)]
        target_dir: Option<std::path::PathBuf>,
    },
    Uninstall {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        target_dir: Option<std::path::PathBuf>,
    },
    Status,
}
```

`doctor` (runs without starting a daemon): `let daemon = DaemonClient::try_existing(&home).await;` then `local_checks(&home, health.as_ref())` plus, when connected, `client.doctor()` (GET `/api/v1/doctor`); print one line per check — `OK   name: detail`, `WARN name: detail — fix`, `FAIL name: detail — fix` — or the JSON report; exit 1 when any check fails. `status`: add lines for the embedding model (and re-embed progress), judge provider and backlog, reviews open, last backup, memory in use. `service install`: `let platform = Platform::current().context("alice service supports macOS, Linux and Windows")?`; `root = target_dir or dirs::home_dir()`; `exe = std::env::current_exe()?` (canonicalised); `uid` from `libc::getuid()` on Unix, 0 elsewhere; `plan_install(…)`; with `--dry-run` print files (path and content) and commands and stop; with `--target-dir` `apply(&plan, None)` and print the commands as "not run (--target-dir)"; otherwise `apply(&plan, Some(&SystemRunner))` and print the notes. `uninstall` mirrors it with `uninstall(..)`. `status` prints `installed (<path>)` or `not installed`.

Process tests (unix), never touching the real service manager:

```rust
#[test]
fn service_install_writes_only_under_the_target_dir_and_dry_run_writes_nothing() {
    let env = Env::new();
    let target = env.home.parent().unwrap().join("svc-root");
    let dry = env.ok(&["service", "install", "--dry-run", "--target-dir", target.to_str().unwrap()]);
    assert!(dry.contains("daemon") && !target.exists(), "{dry}");
    let out = env.ok(&["service", "install", "--target-dir", target.to_str().unwrap()]);
    assert!(out.contains("not run"), "{out}");
    let installed = if cfg!(target_os = "macos") { target.join("Library/LaunchAgents/dev.alice.daemon.plist") } else { target.join(".config/systemd/user/alice.service") };
    assert!(std::fs::read_to_string(&installed).unwrap().contains(&env.home.display().to_string()));
    env.ok(&["service", "uninstall", "--target-dir", target.to_str().unwrap()]);
    assert!(!installed.exists());
}

#[test]
fn doctor_reports_and_exits_non_zero_on_failure() {
    let env = Env::new();
    env.ok(&["remember", "doctor seed"]);
    let report = env.json(&["doctor"]);
    let checks = report["checks"].as_array().unwrap();
    assert!(checks.iter().any(|c| c["name"] == "database" && c["status"] == "ok"));
    // The CLI tests run with ALICE_EMBEDDER=fake and no model: a warning, not a failure.
    assert!(checks.iter().any(|c| c["name"] == "embedding model" && c["status"] == "warn"), "{checks:?}");
    std::fs::write(env.home.join("config.toml"), "[judge]\nreview_threshold = 5\n").unwrap();
    let o = env.run(&["doctor"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains("FAIL config"));
}
```

Run: `nice -n 15 cargo test -j 6 --workspace` — Expected: pass; `pgrep -fl "alice daemon run"` empty; nothing written under `~/Library/LaunchAgents` or `~/.config/systemd` (check with `ls` before and after).

- [ ] **Step 6: Commit**

```bash
nice -n 15 cargo fmt --all && nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
git add crates
git commit -m "feat: alice doctor, fuller health, token rotation and service install for launchd, systemd and Windows

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 18: Docs, spec as built, final verification, push

Bring the README (English and Thai), the spec and the benchmark notes in line with what M2 built, re-check every budget on the final head, run the ignored real-model and live tests once, confirm the agents still work over MCP, and push the branch with CI green on all three OSes. No release, tag or publish.

**Files:**
- Modify: `README.md`, `README.th.md`, `docs/superpowers/specs/2026-10-13-alice-core-design.md`, `docs/benchmarks/m2.md`

**Interfaces:**
- Consumes: every earlier task.
- Produces: documentation only; the pushed branch `feat/m2-judge-settings`.

- [ ] **Step 1: Full local verification on the final head (one heavy run at a time)**

```bash
source "$HOME/.cargo/env" && cd /Users/icue/Code/AI/project-alice
export S=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2 ALICE_HOME=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2/home; mkdir -p "$ALICE_HOME"; [ -e "$ALICE_HOME/models" ] || ln -s "$HOME/.alice/models" "$ALICE_HOME/models"   # scratch prelude (Global Constraints)
nice -n 15 cargo fmt --all --check
nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
nice -n 15 cargo test --workspace -j 6
nice -n 15 cargo test -p alice-daemon --release --test service_concurrency -j 6   # the MemPalace regression, now with the judge and maintenance threads running
ALICE_MODELS_DIR=$HOME/.alice/models nice -n 15 cargo test -j 6 -p alice-embed --release --test real_search_quality -- --ignored --nocapture
ALICE_MODELS_DIR=$HOME/.alice/models nice -n 15 cargo test -j 6 -p alice-embed --release --test binary_chunk_recall -- --ignored --nocapture
ALICE_MODELS_DIR=$HOME/.alice/models nice -n 15 cargo test -j 6 -p alice-embed --release --test onnx_equivalence -- --ignored --nocapture
pgrep -fl "alice daemon run" || echo "no stray daemons"
```

Expected: all green; gold MRR 1.000 and 13/13 top 3; recall@10 of the chunk index as in Task 1; zero token mismatches. Then the Jev live smoke test (fractions of a cent):

```bash
cd /Users/icue/Code/AI/project-alice   # the repo root: `.env` lives here
set -a; . ./.env; set +a
nice -n 15 cargo test -j 6 -p alice-daemon --test jev_live live_compare_round_trip -- --ignored --nocapture
unset TYPESAFE_API_KEY
```

- [ ] **Step 2: The memory budget on the final head**

```bash
cd /Users/icue/Code/AI/project-alice
export S=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2 ALICE_HOME=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/c7c419b0-ad68-422d-b03b-f7b4330c53c2/scratchpad/m2/home; mkdir -p "$ALICE_HOME"; [ -e "$ALICE_HOME/models" ] || ln -s "$HOME/.alice/models" "$ALICE_HOME/models"   # scratch prelude (Global Constraints)
nice -n 15 cargo build --release -j 6 -p alice-memory && cp target/release/alice $S/alice-m2
nice -n 15 $S/alice-m2 bench --real --prefill-vectors --chunks 1000000 --memories 10000 --dir $S/data-1m-prefill --out $S/m2-final-1m.json
nice -n 15 $S/alice-m2 bench --real --chunks 100000 --memories 2000 --dir $S/data-100k-real --out $S/m2-final-100k-real.json
pgrep -fl "alice daemon run" || true
```

Gate (spec §4.6, unchanged): daemon peak RSS (and macOS peak footprint) < 800 MB at 1M chunks with the real model; `remember` p95 < 50 ms, `recall` p95 < 200 ms (also with `include_raw`), `context` p95 < 150 ms; burst errors 0. If the final head misses a budget that Task 2 met, find the task that regressed it (bisect the task commits with the same command) and fix it here; never relax the budget. Commit each regression fix made here (in Step 1 or Step 2) on its own before moving on, with the test or measurement that shows it: `git add <the changed source files>` and `git commit -m "fix: <what regressed> (found by the final M2 check)"` plus the attribution line, then re-run the failing command. Delete `$S/data-*` afterwards (several GB).

- [ ] **Step 3: Agents over MCP (if installed and logged in)**

As in M1 Task 14: a throw-away `ALICE_HOME=$S/agents-home` exported for every command of the step (the model linked into it as in the scratch prelude; the MCP configs pass the same value in `env`), per-invocation configuration only (`claude -p … --mcp-config <file> --strict-mcp-config --allowedTools "mcp__alice__remember,mcp__alice__recall"`; `codex exec -c mcp_servers.alice.command=… -c 'mcp_servers.alice.args=["mcp"]' -c mcp_servers.alice.env={…}`). Check: `remember` stores a memory whose author is the client; a second, near-identical `remember` is folded into the first by the judge (`alice --json get <second id>` answers with the first and a redirect); `recall` finds it; the admin tools are not listed (`claude mcp` tool list or the shim's `tools/list`). Never edit `~/.claude*` or `~/.codex`. Stop the daemon and delete the home. If a CLI is missing or not logged in, record that and move on.

- [ ] **Step 4: Spec as built**

Update `docs/superpowers/specs/2026-10-13-alice-core-design.md` (keep its structure; change what M2 decided or measured):

- §2.3: `secrets.toml` (0600), `vectors/<model>.memories.usearch` + `<model>.chunks.bq`, `backups/` naming (`alice-<stamp>.db`, `alice-pre-migration-v<N>-<stamp>.db`, `alice-before-restore-<stamp>.db`).
- §2.4: ONNX Runtime used directly (`ort`), compact tokenizer in `alice-embed`; `keyring` optional feature.
- §3.3: schema v2 (`events.group_id`, `review_items.dedupe_key`, event/review indexes) and v3 (`memories.kind_explicit`, `project_defaulted`, `judge_attempts`, `memories_judge`).
- §4.1: chunk vectors in a flat sign-bit index rescored from the stored f16 vectors (with the measured recall@10 and memory); usearch without NumKong; index files read and written by Rust.
- §4.2: direct ONNX Runtime with the compact tokenizer (equivalence checked against fastembed), CPU arena off, idle unload (`embedding.idle_unload`, queries lexical-only while it reloads); HTTP providers and their model ids; the re-embed mechanics (phase 1 resumable, phase 2 with the worker paused, atomic switch, old vectors purged by `model_purge`).
- §4.6: the M2 measurement (link `docs/benchmarks/m2.md`): the RSS budget is met; keep every number honest.
- §5.1: activation after judging; `project_defaulted`.
- §5.2: as built — candidates are the 5 nearest memories plus up to 3 title matches (Plan decision 33), kind-classification failures are retried like comparison failures, option shuffle seeded by the request, the converse question form (`outdated_by`), budget reserved before each call (an upper bound: body bytes as tokens, Plan decision 30) and settled after, a spend that cannot be read stops calls (decision 31), connection errors fall back to the heuristic without retry (decision 29), cache keyed by SHA-256 of the exact body, heuristic auto-applies only duplicates and identical-stem predicates, entity proposals deferred to SP2; link `docs/benchmarks/m2-jev.md`.
- §5.3: the detector list as built; a project's `.alice.toml` can only tighten privacy; spooled writes are redacted before they are written.
- §5.4: review type `undo`; the choices per type.
- §5.5: actor `system`; group undo; actions that cannot be undone; pending/active treated as one state.
- §5.6: job list with `model_purge` and `events_prune`; the FTS job runs bounded `merge` steps (500 pages per writer slice) instead of a weekly `optimize`, so the writer is never held for a long stall (Plan decision 32); `alice backup`, `alice maintenance`; one-step online backup; rotation rule; restore steps.
- §6: the keys added (`embedding.idle_unload`, `judge.cluster_similarity`, `[maintenance]`, `[mcp] admin_tools`, `[secrets] backend`), environment naming, restart-only keys, project scope limited to `[privacy]`.
- §8.3 / §8.4: the commands and routes added (`recall --precise`, `history`, `undo`, `review`, `stats`, `config …`, `backup`, `restore`, `maintenance`, `doctor`, `token rotate`, `service …`; `/events`, `/events/stream`, `/undo`, `/reviews…`, `/memories/merge`, `/stats`, `/settings…`, `/secrets/{name}`, `/backups`, `/maintenance…`, `/doctor`, `/admin/token/rotate`); OpenAPI generation and the graph/topics endpoints remain for SP3.
- §11: Windows builds and passes CI; real-machine verification pending.
- §14: add decision rows for: sign-bit chunk index; direct ONNX Runtime + compact tokenizer; idle unload; usearch without NumKong; activation after judging; deterministic shuffle and converse question; budget reservation as an upper bound and fail-closed spend; no retry on connection errors; 5 nearest + up to 3 title matches; bounded FTS merge steps; review type `undo`; project scope privacy-only; unknown `ALICE_*` variables warned, not fatal; restore from the CLI under the daemon lock.

- [ ] **Step 5: READMEs**

`README.md` and `README.th.md` together: status line "M2 complete — alpha"; remove "(planned, M2)" marks for what is now built (self-correcting memory, AI judge, redaction, private secrets file, review inbox, undo, backups, settings, doctor, service install, HTTP embedding providers, Windows build); a Configuration section with a `config.toml` example (`[judge] provider = "jev"`, thresholds, budget; `[privacy]`; `[embedding]` with an `ollama` example; `[maintenance]`), the `ALICE_<SECTION>_<KEY>` rule, `alice config set-secret jev_api_key` (stdin), and the `.alice.toml` privacy keys; the CLI list; the platform table (Windows as in Task 3; Intel macOS / musl: work with an HTTP embedding provider); the performance table with the M2 numbers (memory budget met); a short "Judge" section (heuristic default, Jev optional, costs from `m2-jev.md`); roadmap: M2 done, M3 next. Keep the Thai text natural, not a word-for-word translation, and keep both files' sections in the same order.

- [ ] **Step 6: `docs/benchmarks/m2.md` summary**

Add a top table "Budgets (spec §4.6) on the M2 head" with Step 2's numbers and verdicts, and links to the memory, Windows, maintenance and Jev sections. Note the machine load during the runs, as `m1.md` does.

- [ ] **Step 7: Commit, push, CI**

```bash
nice -n 15 cargo fmt --all --check && nice -n 15 cargo clippy --workspace --all-targets -j 6 -- -D warnings
git add README.md README.th.md docs
git commit -m "docs: M2 as built — spec, READMEs (EN/TH) and benchmarks

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
git push -u origin feat/m2-judge-settings
gh run list --branch feat/m2-judge-settings --limit 1
gh run watch <run-id> --exit-status
```

Expected: ubuntu, macos and windows jobs green. Report per OS. Do not merge, tag or publish; the controller runs the whole-branch review.

---

## Self-review notes (for the executor)

- Every task's first step is "read the files as they are now": the plan was written while M1's final fix wave was committing.
- Event, review and judge code paths share helpers that later tasks extend (`record`, `snap`, `merge`, `supersede`, `ScriptedJev`): when a later task changes a helper's signature (for example the `group` argument in Task 12), update the earlier call sites in the same commit.
- Tests that wait for background work use `wait_settled` (embedding and judge backlogs both zero), never fixed sleeps.

---

## Plan decisions

Decisions taken while writing this plan where the spec or the request left room. Each says what it costs if it turns out wrong.

1. **Chunk vectors move to a flat sign-bit index rescored from the f16 vectors in SQLite** (Task 1), instead of a binary HNSW, a memory-mapped usearch `view` (its touched pages count towards RSS, the very problem M1 fixed for SQLite) or dropping chunk vectors. Memories keep the int8 HNSW. *If wrong:* raw-tier vector search is a linear scan (~48 MB per query at 1M chunks) plus a few hundred primary-key reads; at ~10M chunks it would need a graph again.
2. **The 800 MB budget is measured with prefilled fake vectors under the real model id** (`bench --prefill-vectors`), with the real model loaded and embedding in the daemon. Vectors cost the same whatever their values; embedding 1M real chunks takes hours. *If wrong:* graph shape differences between fake and real vectors could move the memory part's size slightly (memories are a small share at 1M chunks).
3. **fastembed is replaced by ONNX Runtime used directly plus a compact Unigram tokenizer** (Task 2); token ids and vectors are checked against fastembed, which stays a dev-dependency for that check. Literal special-token text such as `</s>` in a passage is tokenised as text. *If wrong:* Alice maintains its own tokenizer for this model; texts containing literal special tokens get slightly different vectors than M1 stored.
4. **If the budget is still missed after Tasks 1–2, the fallback order is fixed**: reader caches 16→8 MiB, writer cache 64→32 MiB, ORT intra threads 4 — each kept only if needed and only if latency budgets hold; otherwise stop and report. *If wrong:* a further structural change (an HTTP-only default) would need the owner's decision.
5. **Idle unload of the local model defaults to 10 minutes** (`embedding.idle_unload`, `0` = never); a query that arrives while it reloads uses word matches only, as the request suggested. *If wrong:* the first recall after a quiet period can miss a vector-only match.
6. **usearch's `numkong` feature is turned off on every platform**, not only Windows, so all OSes run one code path (Task 3 measures the latency cost). *If wrong:* memory-vector distance is computed by usearch's portable code, a few ms slower at large memory counts.
7. **Memory activation moves after the judge stage** (spec §5.1 order); M1 tests that expected `active` right after embedding are updated. `pending` and `active` are treated alike by search, context and undo. *If wrong:* memories stay `pending` until judged (still searchable).
8. **Heuristic judge auto-applies only duplicates and identical-stem predicates**; its kind, routing and "updates" results only feed review items (or nothing, for kind). *If wrong:* offline users get no automatic kinds.
9. **Jev options are shuffled with a seed derived from the request**, so identical decisions build identical bodies and hit the SHA-256 cache; the reversed question asks the converse relation (`outdated_by`) of the same pair in the same request. *If wrong:* the order bias of one question is fixed rather than random across retries; the evaluation reports forward/reversed agreement.
10. **The daily budget reserves an upper bound of each call's cost before the call and settles to actual usage after**, so the day's spend never passes the budget (the bound is decision 30). *If wrong:* the last calls of a day may be refused early.
11. **Judge failures — comparison and kind classification alike — fall back to the heuristic and mark `retry`**; retries run hourly up to 5 attempts, then stop. Inline uses (predicate matching) wait at most 3 s; precise mode waits `judge.timeout_ms` and returns fast results on timeout. *If wrong:* a slow Jev decides fewer predicates than possible.
12. **A judge action whose target changed after it was read becomes a review item** (stamps on every memory the judge saw). *If wrong:* a little more review traffic.
13. **New-entity proposals by the judge are deferred to SP2**: Jev answers choice questions only, so no M2 provider can name a new entity; mentions link known entities. *If wrong:* the graph grows only through `assert_fact` until SP2.
14. **Review type `undo` and actor `system` are added** to the spec's lists (an undo conflict needs a question; maintenance needs an actor). *If wrong:* SP3's UI shows one more type and actor.
15. **Undo reverts a whole event group; purged memories cannot come back (an error, not a review item); events are pruned after 365 days.** *If wrong:* the undo window is one year.
16. **A project's `.alice.toml` may only tighten privacy** (`block_secrets`, `custom_patterns`, `cloud_denied`); other keys are refused. *If wrong:* per-project tuning of search or judge needs a later extension.
17. **`config.toml` is watched by polling every 2 s** (no file-watcher crate). Restart-only keys: `server.port`, `secrets.backend`, `embedding.idle_unload`, `embedding.batch_size`. *If wrong:* edits take up to 2 s; those four keys need `alice daemon stop`.
18. **Environment wins over files**, including `TYPESAFE_API_KEY` / `ALICE_EMBEDDING_API_KEY` over stored secrets (shown as source `env`). *If wrong:* a key set through the UI is masked while the variable is exported.
19. **Raw size retention counts chunk text bytes**, not the database file size; `"0"` is refused as a retention value. *If wrong:* the database can be about twice the configured size.
20. **Backups use one online-backup step on a dedicated read-only connection** (a consistent snapshot; stepwise backups restart on every writer commit); manual backups share the daily/weekly rotation; pre-migration and before-restore copies are kept 30 days plus the newest. *If wrong:* a backup at 1M chunks costs seconds of I/O at once.
21. **Restore runs in the CLI while it holds the daemon lock**, never through a daemon endpoint; writes meanwhile are spooled and drained into the restored database. *If wrong:* nothing — the daemon cannot run while files are swapped.
22. **HTTP embedding providers are part of M2** (spec §4.2/§11 and the README roadmap say M2, §12's M2 row does not list them); vector ids are `<provider>:<model>`, index file names are sanitised. *If wrong:* Task 16 is extra scope.
23. **Model switches use the embedder factory only when the daemon passes one**; a model changed while stopped is re-embedded at start from `meta.active_embedding`; an interrupted re-embed resumes from cursors in `meta`. Two embedders are loaded during a switch. *If wrong:* a local→local switch briefly holds two models.
24. **The Jev evaluation is an ignored test** (`crates/alice-daemon/tests/jev_eval.rs`) with a $2 hard budget and a committed synthetic 200+-pair set, plus a CI check of the set's balance; results go to `docs/benchmarks/m2-jev.md` by hand from the printed tables. *If wrong:* none beyond a manual copy step.
25. **CLI additions beyond spec §8.3**: `history`, `stats`, `backup`, `maintenance`, `config unset|delete-secret|test`, `review show`, and the `ALICE_NO_AUTOSTART` variable (tests and scripts). *If wrong:* a larger CLI surface to keep stable.
26. **Service install**: launchd agent `dev.alice.daemon`, systemd user unit `alice.service`, a Windows logon scheduled task (unverified on a real machine, documented). Tests only use `--dry-run` / `--target-dir`. *If wrong:* the Windows path may need fixes on first real use.
27. **OpenAPI generation and the graph/topics endpoints of §8.4 wait for SP3** (the UI milestone that consumes them); M2's settings API, schema endpoint and SSE stream are the UI contract delivered now. *If wrong:* SP3 starts with that work.
28. **Branch `feat/m2-judge-settings` from the M1 head after its fix wave**; the M1 "should fix before merge" items stay with M1's final review, not this plan. *If wrong:* the controller renames the branch in the commands.
29. **Jev connection errors are not retried** (spec §5.2: retries only on 429, 5xx and timeouts). The call fails at once as `Unavailable`; the heuristic decides, the memory's `judge_state` becomes `retry`, and the hourly `judge_retry` job asks again. *If wrong:* a momentary network blip delays the model's view of that memory by up to an hour (the heuristic's decision stands meanwhile); in exchange no time or budget is spent hammering a host that cannot be reached.
30. **A call's budget reservation is the request body's length in bytes taken as input tokens** — a guaranteed upper bound, since a token never covers less than one byte — multiplied by the highest tokens-per-byte ratio Jev has reported if that ever exceeds 1. The day's spend therefore never passes `judge.daily_budget_usd`. *If wrong:* typical JSON runs 3–4 bytes per token, so the bound is 3–4× the real cost and the day's last call (worth a fraction of a cent at $0.042 per million tokens) can be refused while room remains; and if Jev counts tokens the body does not contain (a prompt of its own), the first such call can overshoot by that difference once before the ratio adapts.
31. **A spend that cannot be read counts as an exhausted budget** (fail closed): the Jev judge refuses calls with `JudgeError::Budget`, the heuristic decides, health shows "cannot read today's Jev spend", and the spend is read again on the next call. *If wrong:* a transient read error pauses Jev for a while (memories judged meanwhile are marked `retry`); it can never let a day spend a second budget, which a fall-back to 0 would.
32. **FTS maintenance runs bounded `merge` steps instead of a weekly `optimize`** (Task 14's `fts_optimize` job: `INSERT INTO t(t, rank) VALUES ('merge', 500)` per writer slice until no work is left, pausing when a request arrives). `optimize` rewrites the whole index in one write transaction, which at 1M chunks would hold Alice's single writer for seconds to minutes and stall every `remember`. *If wrong:* the index can be left with more segments than a full optimize leaves (FTS queries a little slower until later merges), and a large index takes several idle periods to merge fully; spec §5.6 says "FTS optimize" and is updated in Task 18.
33. **The judge compares a new memory with its 5 nearest live memories by vector plus up to 3 more of the same kind whose titles are nearly the same** (Task 12 `candidates`). Spec §5.1 says "the 5 nearest"; title matches catch an update that is worded differently enough to fall outside the vector top 5 (the heuristic's "updates" signal). *If wrong:* up to 8 instead of 5 comparisons per memory — a larger Jev request (more tokens per write) and a little more chance of a spurious review item; spec §5.2 is updated in Task 18.
