# Alice SP2 — Automatic Capture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Capture every Claude Code (then Codex) session into Alice's raw tier, locally and on server tenants, and distil
ended sessions into a few judged knowledge memories with Jev, without floods, duplicates, blocking or leaks.

**Architecture:**
- **One shared core** (`alice-core::capture`) turns transcript bytes into rendered, deduplicated turns. The daemon writes
  each turn's chunks, its turn key and the cursor in one writer transaction.
- **Locally,** `alice hook <event>` only notifies the daemon, and the daemon reads the file.
- **On a tenant,** a POSIX `sh` script posts the new bytes, and the server keeps the cursor.
- **A distill worker** turns new chunks into extractive candidates, asks Jev one `choice` request per run, and writes
  `origin = distill` memories through the normal `remember` path. The judge's rules for those memories keep them out of
  the review inbox.

**Tech Stack:** Rust 1.99 (edition 2024), rusqlite (SQLite + FTS5), axum, tokio, reqwest, serde/serde_json, toml_edit,
POSIX `sh` + `curl` for the tenant scripts. No new dependency.

**Spec:** `docs/superpowers/specs/2026-10-16-alice-sp2-capture-design.md` (read it whole first; § numbers below refer to
it). Also binding: the core spec `2026-10-13-alice-core-design.md`, the server-mode spec
`2026-10-15-alice-server-mode-design.md`, and the lane brief `/Users/icue/Code/AI/project-alice/.superpowers/lanes/sp2-lane.md`
with its review `sp2-spec-review.md`.

## Global Constraints

**Toolchain and code**
- Toolchain: Rust stable 1.99, edition 2024, `resolver = "3"` (pinned in `rust-toolchain.toml`).
- Crates: `alice-core`, `alice-embed`, `alice-daemon`, and the binary package `alice-memory` (`[[bin]] name = "alice"`).
- **No new dependency, and no new feature on an existing one.** In particular, `serde_json/preserve_order` must not be
  turned on: it would change JSON output across the workspace. `toml_edit` is already a workspace dependency.
- IDs are ULIDs. Timestamps are RFC 3339 UTC with milliseconds and `Z`, stored as TEXT.
- Error text for agents and CLI users is one or two sentences: what happened and what to do next.

**Database and writer**
- Only the daemon opens `alice.db`.
- All writes go through the single writer (`WriteOp`). Writer slices stay at most 50 chunks / 32 KiB (`INGEST_SLICE`,
  `INGEST_SLICE_BYTES`), and maintenance deletes at most 500 rows per op.
- Network calls never run on the writer thread or on an async runtime thread (`spawn_blocking` or a worker thread).

**Budgets, never relaxed:**
- `remember` ack p95 < 50 ms;
- `recall` fast p95 < 200 ms at 1M chunks;
- `context` p95 < 150 ms;
- `alice mcp` start < 20 ms;
- daemon RSS < 800 MB;
- **`alice hook <event>` p95 < 100 ms from process start to exit**, daemon up and daemon down.

**Recall quality never drops:** `crates/alice-core/tests/search_quality.rs` and the ignored real-model
`crates/alice-embed/tests/real_search_quality.rs` keep passing with unchanged thresholds.

**Limits.** These stand: content ≤ 65,536 bytes, title ≤ 200 chars, ≤ 20 topics, importance 1–5, chunk ≤ 32,768 bytes,
≤ 1,000 chunks per ingest.

**HTTP.** The daemon binds 127.0.0.1. Every route needs the bearer token and passes the Host/Origin guard, except
`/api/v1/ping` and the two public scripts of Task 15.

**Secrets**
- Never read, print, grep or source `.env`. Never print `TYPESAFE_API_KEY`; check a file only with
  `grep -c -F -- "$TYPESAFE_API_KEY" <file>` (must print `0`).
- Tests run with `env -u TYPESAFE_API_KEY -u ALICE_EMBEDDING_API_KEY`.
- Secret-shaped test strings are built with `concat!` split inside the provider prefix.
- Recorded Jev fixtures hold request and response bodies only, never headers.
- Unit and integration tests never touch the network. Live tests are `#[ignore]` and read keys from the environment only.

**The owner's files.** Never touch `~/.alice`, `~/.alice.pre-*`, `/Users/icue/Code/AI/memory_vault`, `~/.mempalace`,
`~/.claude*` or `~/.codex`. The only exception is Task 1's read-only copies and Task 20's real-session copies, under the
controller's conditions.
- Every test, and every manual step, sets `HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `ALICE_HOME` to throwaway
  directories.
- `alice hooks install` is never run against a real home.

**The repository is public.** Never write the server's address, a password or a token into a committed file. Do not use
the server; pane 3 owns it.

**Scratch prelude** for every manual or shell step:
`export S=/private/tmp/claude-501/-Users-icue-Code-AI-project-alice/ab72369b-0a6f-46c4-ae27-998c4cb9ec45/scratchpad/sp2 ALICE_HOME=$S/home HOME_T=$S/fakehome; mkdir -p "$ALICE_HOME" "$HOME_T"`.
Never `rm -rf` an unguarded variable; use `"${VAR:?}"`.

**Builds and tests**
- Throttle: `source "$HOME/.cargo/env"` first, then `nice -n 15 cargo … -j 4`. One heavy run (build, suite, bench, eval)
  at a time.
- Before every commit: the task's tests, `cargo fmt --all --check`, and
  `cargo clippy --workspace --all-targets -- -D warnings`.
- The workspace builds and every test passes after **every** task (`cargo test --workspace` compiles ignored tests too).
- CI is ubuntu and macOS only. A test that needs Unix is `#[cfg(unix)]`.
- Process tests leave no stray daemons: after a run, `pgrep -fl "alice daemon run"` shows nothing the run started.

**Git**
- Stage by name; never `git add -A`. Commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- Push only `feat/sp2-capture`; never merge into `main` or another lane's branch.
- When `origin/feat/server-mode` moves, merge it in at a task boundary and re-run the suite.
- Rustfmt can move an edit anchor. **Every implementer confirms each named test exists in the commit with `git show --stat`
  (and a grep for the test's name) before reporting.**

**Docs.** English. `README.th.md` changes in the same commit as `README.md`.

**Process**
- No release, tag or publish.
- Diary entries are the controller's, not the lane's.
- Subagents run on `model: "sonnet"` before 2026-10-16 17:00 Asia/Bangkok. Opus is allowed after that, for reviews.

## Review Focus

The inputs and conditions the spec implies but that no named test below would otherwise exercise. Each has a test in its
owning task.

1. **A transcript line in a shape a newer agent version writes:** content as a string where an array is expected, an
   unknown block type, a missing `uuid`, or `null` fields. The reader skips and counts the line and never panics. Pinned in
   Task 2 by `reader_survives_arbitrary_json_lines`.
2. **A file that ends mid-line, mid-UTF-8 sequence, or with invalid UTF-8 in one line:** only complete lines are consumed,
   the invalid line is skipped, and the cursor always lands on a line start. Pinned in Task 2 by
   `cursor_only_ever_lands_on_a_line_start`.
3. **Ten or more panes stopping at once:** notifications for 20 sessions arrive together while agents `remember`.
   Passes are coalesced per source, nothing is lost or doubled, and `remember` stays within its budget's order of
   magnitude. Pinned in Task 6 by `twenty_sessions_at_once_capture_everything_once_beside_remembers`.
4. **A very large transcript (a backfill of a 40 MB file):** it is read in bounded passes of at most 8 MiB, and every turn
   arrives once. Pinned in Task 6 by `a_forty_megabyte_transcript_is_captured_in_bounded_passes`.
5. **Non-ASCII and awkward paths:** a Thai working-directory name with spaces and quotes, in the hook JSON. The local hook
   passes it through intact, and the tenant script handles raw UTF-8 and skips only `\u` escapes. Pinned in Task 7 by
   `hook_passes_a_thai_path_with_spaces_and_quotes`, and in Task 15 by `hook_script_handles_utf8_and_skips_u_escapes`.

## Plan decisions

These clarify the spec where it left the choice to the builder. Each line gives the decision, why, and the cost if
it is wrong.

| # | Decision | Why | Cost if wrong |
|---|---|---|---|
| P1 | **Capture verdict has three answers.** `Capture`; `Excluded` (skip the turns, advance the cursor, record the keys; spec §3.6); `Wait` (store nothing, keep the cursor). `Wait` applies while `SettingsHandle::project_fail_closed(root)` or `config_degraded()` | A fail-closed state means the rules are unknown right now. Moving past the turns would lose them for good (the spool's rule: fail-closed must not destroy data) | A project whose `.alice.toml` stays broken is re-read each sweep and reported by `alice doctor` until it is fixed |
| P2 | **The capture-scoped retention is its own maintenance job,** `capture_retention` (daily), rather than a phase of `raw_retention` | The job has its own status line, `alice maintenance run capture_retention`, and its own error state. The spec's behaviour is unchanged | One more job name in `alice maintenance status` |
| P3 | **Jev for distillation:** a new trait method `Judge::ask_distill(projects, state, questions, share) -> JudgeResult<BTreeMap<String, Distribution>>`. Its default returns `Denied`; `JevJudge` implements it. Budget state is per consumer inside `JevLimits` | The distiller reuses the gate, redaction, cache, rate limit and reservation unchanged, and `JudgeHub::current()` already hands out the judge | None beyond one trait method |
| P4 | **The hook installer lives in `alice-daemon`** (`agent_hooks.rs`) | The `alice doctor` check reads the same files with the same rules | — |
| P5 | **`settings.json` is edited through a small order-preserving JSON value** (`agent_hooks/json.rs`: parse, then print with 2-space indent). The result is re-parsed with `serde_json` and compared with the intended value before the atomic write | Spec R20 keeps key order, and `preserve_order` is off-limits | About 200 lines of parser. The round-trip check refuses to write anything it cannot reproduce |
| P6 | **Hook timeouts:** ping 30 ms, notify 40 ms, total deadline 90 ms (`HOOK_DEADLINE`); the briefing gets what is left | Spec §4.1 | A daemon that is merely slow at session start gives no briefing; the agent calls `context` itself |
| P7 | **The tenant script's `.alice.toml` check** is two `grep -E` patterns on the file at the git top-level (else `cwd`): `^[[:space:]]*no_capture[[:space:]]*=[[:space:]]*true` and the same for `cloud_denied`. An unreadable file skips the event | Spec §5.3 step 6; a fail-closed `sh` check without a TOML parser | A `no_capture` written in another TOML form (e.g. inline table) is not seen; documented |
| P8 | **The transcript route on a non-tenant daemon** takes the project from a `cwd` query parameter, like the notify path. `project` is a hint only in tenant mode, as `Service::choose` already does | Tests and the adoption test run the route locally | — |
| P9 | **Distilled `preference_user` memories** go to `global` as `ProjectChoice::Slug("global")` | Spec §6.4 | — |
| P10 | **Codex is behind its sample:** Task 17 runs only if Task 1 found a Codex rollout. Otherwise Task 17 ships the seam only: `AgentKind::Codex` exists, `alice hooks install codex` refuses with "Codex capture waits for a transcript sample", and the tenant setup skips Codex | The controller's condition (spec §13) | Codex users get MCP only until a sample exists |

---

## File structure

**`alice-core` (pure logic and storage):**

| File | Status | Responsibility |
|---|---|---|
| `crates/alice-core/src/capture/mod.rs` | new | `AgentKind`, `ReadOptions`, `ReadOutcome`, `CapturedTurn`, `RenderedChunk`, `read()`, turn keys, constants |
| `crates/alice-core/src/capture/claude.rs` | new | Claude Code JSONL lines → `Record`s |
| `crates/alice-core/src/capture/codex.rs` | new (Task 17) | Codex rollout lines → `Record`s |
| `crates/alice-core/src/capture/record.rs` | new | `Record`, `Block`, turn grouping and completeness |
| `crates/alice-core/src/capture/render.rs` | new | the rendering table, trimming, chunk cutting |
| `crates/alice-core/src/distill/mod.rs` | new | `RunInput`, `Candidate`, `candidates()`, `request()`, `decide()`, `DistilledMemory` |
| `crates/alice-core/src/store/capture.rs` | new | `capture_state` / `capture_turns` SQL |
| `crates/alice-core/src/store/sql/0006_capture.sql` | new | migration (spec §8.1) |

**`alice-core` (changed):**

| File | Change |
|---|---|
| `crates/alice-core/src/store/mod.rs` | migration list, `pub mod capture` |
| `crates/alice-core/src/store/memories.rs` | `origin`, provenance insert |
| `crates/alice-core/src/store/events.rs` | `Actor::Distill` |
| `crates/alice-core/src/store/maintenance.rs` | capture-chunk retention helpers |
| `crates/alice-core/src/model.rs` | `Origin`, `Memory.origin` |
| `crates/alice-core/src/api.rs` | capture wire types, `Hit.origin`, `Health.capture`, `Stats` fields |
| `crates/alice-core/src/settings.rs` | `[capture]`, `[distill]`, `ProjectPrivacy.no_capture`, loosening |
| `crates/alice-core/src/judge/mod.rs` | `JudgeMemory.origin`, `Relation.existing_origin`, `Judge::ask_distill` |
| `crates/alice-core/src/judge/pipeline.rs` | distilled-newcomer rules |
| `crates/alice-core/src/judge/jev.rs` | consumer budget, ledger provider |
| `crates/alice-core/src/context.rs` | `(auto)`, section rules, hide unjudged |
| `crates/alice-core/src/search/mod.rs` | hide unjudged distilled; `origin` on hits |
| `crates/alice-core/src/import/plan.rs` | skip sessions held by capture |

**`alice-daemon` (new):**

| File | Responsibility |
|---|---|
| `crates/alice-daemon/src/capture.rs` | `CaptureCtl`: queue, worker thread, passes, sweep, upload, status, briefing |
| `crates/alice-daemon/src/distill.rs` | `DistillCtl`: worker thread, triggers, breaker, runs |
| `crates/alice-daemon/src/agent_hooks.rs` | install / uninstall / status of agent hook configs |
| `crates/alice-daemon/src/agent_hooks/json.rs` | the order-preserving JSON value |
| `crates/alice-daemon/scripts/capture/setup.sh`, `crates/alice-daemon/scripts/capture/hook.sh` | tenant scripts, `include_str!`ed |

**`alice-daemon` (changed):**

| File | Change |
|---|---|
| `crates/alice-daemon/src/writer.rs` | `CaptureSlice`, `CaptureStateUpdate`, `Remember` origin/provenance, new judge actions, ledger provider |
| `crates/alice-daemon/src/service.rs` | holds `CaptureCtl`/`DistillCtl`; capture methods; health |
| `crates/alice-daemon/src/http.rs` | capture routes |
| `crates/alice-daemon/src/spool.rs` | `SpoolEntry::CaptureNotify` |
| `crates/alice-daemon/src/client.rs` | hook client |
| `crates/alice-daemon/src/judge_http.rs` | spent across both rows |
| `crates/alice-daemon/src/judge_worker.rs` | origins, stand-in rule |
| `crates/alice-daemon/src/maintenance.rs` | `capture_retention` job; `merge_suggestions` skips distilled |
| `crates/alice-daemon/src/doctor.rs` | capture and distill lines |
| `crates/alice-daemon/src/importer.rs` | captured session ids |

**The `alice` binary:**

| File | Responsibility |
|---|---|
| `crates/alice/src/hook.rs` (new) | the `alice hook` fast path |
| `crates/alice/src/cmd.rs`, `crates/alice/src/main.rs` | `hook`, `hooks`, `capture` subcommands |

**Tests and fixtures:**
- `crates/alice-core/tests/fixtures/capture/` (Claude Code and Codex fixtures);
- `crates/alice-daemon/tests/capture_flow.rs`, `capture_tenant.rs`, `capture_scripts.rs`, `distill_flow.rs`,
  `distill_eval.rs`;
- `crates/alice-daemon/tests/fixtures/distill/`;
- `crates/alice/tests/cli.rs` (hook and installer tests).

**Docs:**
- `docs/capture.md` (new);
- `docs/benchmarks/sp2.md` and `docs/benchmarks/sp2-distill.md` (new);
- `README.md` and `README.th.md`;
- `docs/server.md`.

---

## Task 1: Structure-only transcript samples (the controller's conditions)

**Files:**
- Create: `crates/alice-core/tests/fixtures/capture/claude/sample-1.jsonl`,
  `crates/alice-core/tests/fixtures/capture/claude/sample-2.jsonl`, `crates/alice-core/tests/fixtures/capture/claude/FORMAT.md`
- Create (only if a rollout exists): `crates/alice-core/tests/fixtures/capture/codex/sample-1.jsonl`,
  `crates/alice-core/tests/fixtures/capture/codex/FORMAT.md`
- Create (not committed): `$S/samples/` (the copies), `$S/scrub.py`, `$S/overlap_check.py`
- Ledger: `.superpowers/sdd/2026-10-16-alice-sp2-capture/progress.md`

**Interfaces:**
- Produces:
  - the committed fixtures;
  - each `FORMAT.md`, listing every line `type`, every field seen with its JSON type, every content-block `type`, and how
    `isMeta`, `isSidechain`, `isCompactSummary`, `compact_boundary`, `message.id` splits and tool results appear;
  - Codex: the `type`/`payload` mapping.
  Tasks 2, 3 and 17 rely on these files.

- [ ] **Step 1: Pick and copy.** Choose two Claude Code transcripts under `~/.claude/projects/` with tool use, at least one
  compaction and a subagent (`<session>/subagents/agent-*.jsonl`) if one exists. Choose one Codex rollout under
  `~/.codex/sessions/` if any exists. Copy them read-only (`cp`, then `chmod 0400`) into `$S/samples/`. Never open the
  originals for writing.
- [ ] **Step 2: Write `$S/scrub.py`** (run with `python3 -I`). For every JSON line it:
  - keeps keys, JSON types, line order and structure;
  - replaces every string value that is text with `lorem-<n>` of similar length;
  - rewrites `uuid`/`parentUuid`/`sessionId`/`message.id`/`tool_use_id`/`agent_id` consistently to fresh UUIDs (the same
    input gives the same output);
  - shifts timestamps by a fixed random offset, keeping their order;
  - rewrites `cwd` and paths to `/home/user/project-x/…`, and the project name to `project-x`;
  - keeps tool names, block types, `type`/`subtype`, booleans and numbers.
- [ ] **Step 3: Write `$S/overlap_check.py`.** It collects every string value of 16 or more characters from the copies.
  Exit 1 if any substring of 16 or more characters of one of them appears in a fixture; print counts only, never the text.
- [ ] **Step 4: Run the scrub and both checks.**
  Run: `python3 -I $S/scrub.py $S/samples crates/alice-core/tests/fixtures/capture && python3 -I $S/overlap_check.py $S/samples crates/alice-core/tests/fixtures/capture; for f in crates/alice-core/tests/fixtures/capture/*/*.jsonl; do grep -c -F -- "$TYPESAFE_API_KEY" "$f"; done`
  Expected: the overlap check prints `overlaps: 0` and exits 0; every `grep -c` prints `0`.
- [ ] **Step 5: Write each `FORMAT.md`** from the fixtures (structure only, no original text).
- [ ] **Step 6: Delete the copies and record.** Run `rm -rf "${S:?}/samples"`. Record in the ledger: the files chosen (as
  "transcript 1/2", never their paths or names), the check's output, and whether a Codex rollout existed (P10).
- [ ] **Step 7: Commit**
  ```bash
  git add crates/alice-core/tests/fixtures/capture/claude/sample-1.jsonl crates/alice-core/tests/fixtures/capture/claude/sample-2.jsonl crates/alice-core/tests/fixtures/capture/claude/FORMAT.md
  git commit -m "test(sp2): structure-only Claude Code transcript fixtures (Task 1)"
  ```
  Add the Codex files to the same commit if they exist.

---

## Task 2: Records and turns (Claude Code reader)

**Files:**
- Create: `crates/alice-core/src/capture/mod.rs`, `crates/alice-core/src/capture/record.rs`,
  `crates/alice-core/src/capture/claude.rs`
- Create: `crates/alice-core/tests/fixtures/capture/claude/{turns.jsonl,compaction.jsonl,continuation.jsonl,late_tail.jsonl,odd_lines.jsonl}`
  (hand-written, in the shape `FORMAT.md` documents)
- Modify: `crates/alice-core/src/lib.rs` (`pub mod capture;`)

**Interfaces:**
- Consumes: Task 1's `FORMAT.md`.
- Produces (`alice_core::capture`):
  ```rust
  pub const MAX_LINE_BYTES: usize = 4 << 20;   // a longer line is skipped with a stub
  pub const MAX_PASS_BYTES: usize = 8 << 20;   // one pass reads at most this much
  pub enum AgentKind { ClaudeCode, Codex }     // as_str(): "claude-code" | "codex"; parse(&str) -> Option<Self>
  impl AgentKind { pub fn source_uri(self, session: &str, subagent: Option<&str>) -> String }
      // "claude-code:session:<s>" / "claude-code:session:<s>:agent:<a>" / "codex:session:<s>"
  pub fn valid_id(s: &str) -> bool             // ^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$
  pub struct ReadOptions { pub tool_output_chars: usize, pub chunk_chars: usize, pub include_thinking: bool,
                           pub at_file_start: bool, pub close_trailing: bool }
  pub struct ReadOutcome { pub turns: Vec<CapturedTurn>, pub tail: Vec<RenderedChunk>, pub consumed: u64,
                           pub oversized_line: bool, pub stats: ReadStats }
  pub struct CapturedTurn { pub key: String, pub started_at: Option<String>, pub prompt: Option<String>,
                            pub chunks: Vec<RenderedChunk> }
  pub struct RenderedChunk { pub text: String, pub authored_at: Option<String> }
  pub struct ReadStats { pub lines: u64, pub unknown: u64, pub invalid: u64, pub too_large: u64 }
  pub fn read(agent: AgentKind, input: &[u8], opts: &ReadOptions) -> ReadOutcome
  pub fn turn_key(first_uuid: Option<&str>, timestamp: Option<&str>, first_text: &str) -> String
      // the uuid when present; else hex(sha256(timestamp ‖ "\x1f" ‖ first 200 chars))[..32]
  ```
  - In this task, `read()` renders through a placeholder renderer (one chunk per turn, its blocks joined by `\n`). Task 3
    replaces it.
  - `CapturedTurn::prompt` is the first sentence of the turn's prompt, at most 200 characters (the distiller's context
    line).

- [ ] **Step 1: Write the failing tests** in `crates/alice-core/src/capture/mod.rs` (`#[cfg(test)]`). Fixtures are read
  with `include_bytes!("../../tests/fixtures/capture/claude/<file>")`.
  - **`turns_start_at_real_prompts_only`:** in `turns.jsonl`, 3 prompts, tool results in `user` records, an `isMeta`
    record and a sidechain record make exactly 3 turns. Each turn's `key` equals its prompt record's `uuid`.
  - **`trailing_turn_waits_unless_closed`:** with `close_trailing: false`, `read` returns 2 turns, and `consumed` equals
    the byte offset of the third prompt's line. With `true` it returns 3 turns, and `consumed == input.len()`.
  - **`compaction_summary_is_its_own_turn`:** `compaction.jsonl` yields a turn keyed by the `isCompactSummary` record's
    uuid. The `compact_boundary` line yields nothing.
  - **`late_records_at_a_cursor_become_the_tail`:** `late_tail.jsonl` (an assistant line and a tool result, then a prompt)
    with `at_file_start: false` gives `tail.len() == 1` and 1 turn. With `at_file_start: true` the head records form their
    own turn, keyed by the first record.
  - **`streaming_duplicates_render_once`:** two lines with the same `message.id` and the same text block render that text
    once.
  - **`reader_survives_arbitrary_json_lines`** (Review Focus 1). Each line of `odd_lines.jsonl` has a different odd shape:
    string content, an unknown block type, a null `message`, a number where an object is expected, a missing `uuid`, or a
    non-object JSON value. The read does not panic. `stats.unknown + stats.invalid` equals the odd lines that carry no text,
    and the readable ones still form turns.
  - **`cursor_only_ever_lands_on_a_line_start`** (Review Focus 2). For every prefix length of `turns.jsonl` (cut at every
    byte, including inside a multi-byte UTF-8 sequence), with `close_trailing` both ways, `consumed` is 0 or an offset
    right after a `\n`. A line with invalid UTF-8 is counted in `stats.invalid` and skipped.
  - **`a_line_over_four_mebibytes_leaves_a_stub`:** a 5 MiB assistant line renders `[record skipped: too large]` in its
    turn.
  - **`no_newline_in_a_full_pass_is_an_oversized_line`:** a slice of `MAX_PASS_BYTES` bytes with no `\n` gives
    `oversized_line == true` and `consumed == 0`.
  - **`the_samples_read_without_unknown_lines`:** Task 1's `sample-1.jsonl` and `sample-2.jsonl` read with
    `close_trailing: true` have `stats.unknown == 0` (or every unknown type is listed as deliberately dropped in
    `FORMAT.md`), and at least one turn each.
- [ ] **Step 2: Run them to see them fail.** Run:
  `cd /Users/icue/Code/AI/project-alice-sp2 && source "$HOME/.cargo/env" && env -u TYPESAFE_API_KEY -u ALICE_EMBEDDING_API_KEY nice -n 15 cargo test -p alice-core -j 4 capture::`
  Expected: compile errors (module missing).
- [ ] **Step 3: Implement.**
  - **`record.rs`:**
    - `Record { uuid: Option<String>, message_id: Option<String>, timestamp: Option<String>, kind: RecordKind, blocks: Vec<Block> }`;
    - `RecordKind { Prompt, Assistant, ToolResults, Summary, Skip }`;
    - `Block { Text(String), Thinking(String), ToolUse { id: String, name: String, input: serde_json::Value }, ToolResult { tool_use_id: String, text: String, is_error: bool }, Image, Document, Audio, TooLarge }`;
    - `fn group(records) -> (Vec<RawTurn>, tail)`, applying the turn rule and completeness of spec §3.3.
  - **`claude.rs`:** `fn parse_line(line: &[u8]) -> LineResult` (`Record` | `Unknown` | `Invalid`). It uses
    `serde_json::Value`, not derived structs, so unknown shapes degrade instead of failing.
  - **`mod.rs`:** splits complete lines, enforces `MAX_LINE_BYTES` and `MAX_PASS_BYTES`, and computes `consumed`.
- [ ] **Step 4: Run them to see them pass.** Run the command of Step 2. Expected: all `capture::` tests pass.
- [ ] **Step 5: Commit** (`git add` the three source files, `lib.rs` and the five fixtures):
  `feat(sp2): Claude Code transcript records and turns (Task 2)`.

---

## Task 3: Rendering and chunk cuts

**Files:**
- Create: `crates/alice-core/src/capture/render.rs`
- Create: `crates/alice-core/tests/fixtures/capture/claude/{tools.jsonl,injected.jsonl,binary.jsonl}`
- Modify: `crates/alice-core/src/capture/mod.rs` (call `render::turn`)

**Interfaces:**
- Consumes: Task 2's `Record`/`Block`/`RawTurn`.
- Produces:
  - `pub(crate) fn turn(raw: &RawTurn, opts: &ReadOptions) -> Vec<RenderedChunk>`;
  - `pub fn trim(text: &str, keep: usize) -> String` (head 5/8, tail 3/8, at UTF-8 boundaries, marker
    `[… {n} characters trimmed …]` with `n` grouped by thousands);
  - `pub const TAGS: &[&str] = &["[user]", "[assistant]", "[tool ", "[result]", "[result error]", "[summary]", "[subagent "]`
    (the contract the distiller parses, spec §3.3).
  - The memory tools whose input becomes a title and whose result is omitted:
    `pub const MEMORY_TOOLS: &[&str] = &["remember", "recall", "context", "get_memory", "update_memory", "query_facts", "assert_fact", "mempalace_"]`.
    These are matched as the last `__` segment of an MCP tool name (`mcp__alice__remember` → `remember`); `mempalace_`
    matches as a prefix.

- [ ] **Step 1: Write the failing tests** in `render.rs`:
  - **`every_rendering_row_of_the_spec_table`** (`tools.jsonl`), each tool rendered as spec §3.3 says:
    ```rust
    assert!(out.contains("[tool Read] /home/user/project-x/src/main.rs") && !out.contains("fn main"));
    assert!(out.contains("[tool Edit] /home/user/project-x/src/lib.rs") && !out.contains("old_string"));
    assert!(out.contains("[tool Bash] git status --short"));
    assert!(out.contains("[… ") && out.contains(" characters trimmed …]"));   // a 5,000-char Bash result
    assert!(out.contains("[tool alice remember] Use pnpm, not npm") && out.contains("[result omitted: memory tool]"));
    assert!(!out.contains("TodoWrite"));
    assert!(out.contains("[subagent general-purpose]"));
    ```
  - **`system_reminders_and_hook_context_are_stripped`** (`injected.jsonl`): no `<system-reminder>`, no CLAUDE.md text,
    and no `# Context · ` briefing text in the output.
  - **`images_binaries_and_thinking_never_stored`** (`binary.jsonl`): `[image]` is present. No 200-character base64 run is
    present (`[binary removed]` instead). The thinking text is absent by default and present with
    `include_thinking: true`.
  - **`trim_keeps_head_and_tail_at_char_boundaries`:** `trim` of 3,000 Thai characters with `keep = 800` is valid UTF-8,
    starts with the first 500 characters, ends with the last 300, and has the marker between them.
  - **`chunks_respect_chunk_chars_and_cut_at_blocks`:** with `chunk_chars: 500`, every chunk is at most 500 characters. A
    2,000-character assistant block is cut at paragraph ends, and the continuation chunks start `[assistant] (cont.)`.
  - **`rendering_is_deterministic`:** rendering the same turns twice is byte-equal, and so is rendering from a re-read
    starting at the turn's offset.
  - **`chunk_authored_at_is_its_first_record_time`.**
- [ ] **Step 2: Run** `… cargo test -p alice-core -j 4 capture::render`. Expected: FAIL.
- [ ] **Step 3: Implement `render.rs`.**
  - Block rendering follows the table of spec §3.3, with tool names looked up from `ToolUse.id` for each `ToolResult`.
  - Cutting: blocks are packed greedily into chunks of at most `chunk_chars` characters. An oversized block is cut at
    `\n\n`, then at `. `/`。`/`\n`, then hard at `chunk_chars`. Each chunk stays at most 32,768 bytes.
- [ ] **Step 4: Run** the Task 2 and Task 3 tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(sp2): render transcript turns into trimmed chunks (Task 3)`.

---

## Task 4: Settings for capture and distillation

**Files:**
- Modify: `crates/alice-core/src/settings.rs` (`CaptureSettings`, `DistillSettings`, `ProjectPrivacy`,
  `EffectivePrivacy`, `ProjectSettings::fail_closed`, validation, `loosened_protections`, key infos)
- Modify: `crates/alice-daemon/src/config.rs` (`SettingsHandle::capture_verdict`)

**Interfaces:**
- Produces:
  ```rust
  pub struct CaptureSettings {            // existing six agent flags kept, plus:
      pub enabled: bool,                  // true
      pub excluded_projects: Vec<String>, // []
      pub include_subagents: bool,        // true
      pub include_thinking: bool,         // false
      pub tool_output_chars: usize,       // 800, valid 0..=8000
      pub chunk_chars: usize,             // 2000, valid 500..=8000
      pub briefing: bool,                 // true
      #[schemars(with = "String")] pub raw_retention: Retention, // Forever; same parse and limits as storage.raw_retention
  }
  pub enum DistillProvider { #[default] Off, Jev }     // serde "off" | "jev"
  pub struct DistillSettings { pub provider: DistillProvider, pub model: Option<String>, pub min_confidence: f64 /*0.7, 0.5..=0.99*/,
      pub max_per_run: usize /*8, 1..=50*/, pub idle_minutes: u32 /*15, 1..=1440*/, pub importance: u8 /*2, 1..=3*/,
      pub budget_share: f64 /*0.5, 0.05..=1.0*/ }
  pub struct ProjectPrivacy { /* … */ pub no_capture: bool }      // tightening only
  pub struct EffectivePrivacy { /* … */ pub no_capture: bool }
  // alice-daemon config.rs
  pub enum CaptureVerdict { Capture, Excluded(&'static str), Wait(String) }
  impl SettingsHandle { pub fn capture_verdict(&self, agent: AgentKind, project_slug: &str, root: Option<&Path>) -> CaptureVerdict }
  ```
  `capture_verdict`'s order:
  1. `Wait` if `config_degraded()` or `project_fail_closed(root)`;
  2. `Excluded("capture is off")` if `!enabled`;
  3. `Excluded("this agent's capture is off")` for the agent flag;
  4. `Excluded("project excluded")` for `excluded_projects` (slugified on both sides, as the cloud list is);
  5. `Excluded("the project's .alice.toml says no_capture")`;
  6. else `Capture`.

- [ ] **Step 1: Write the failing tests** (`settings.rs` and `config.rs` test modules):
  - **`capture_and_distill_defaults_match_the_spec`:** every default listed above. `toml::from_str("[distill]\nprovider = \"jev\"")`
    parses to `DistillProvider::Jev`, and `"maybe"` is refused, naming the key.
  - **`capture_ranges_are_validated`:** `tool_output_chars = 9000`, `chunk_chars = 100`, `min_confidence = 0.4`,
    `max_per_run = 0`, `importance = 4` and `budget_share = 0` each give a `SettingError` for that key, and the previous
    value stays.
  - **`capture_raw_retention_parses_like_storage_retention`:** `"180d"`, `"1MB"` and `"forever"` are accepted; `"0"` and
    `"12h"` are refused. `ALICE_CAPTURE_RAW_RETENTION=180d` in the env layer wins over `config.toml`.
  - **`new_loosening_keys_are_named`:** `loosened_protections(old, new)` names exactly:
    - `capture.enabled` false→true;
    - `capture.codex` false→true;
    - an entry removed from `capture.excluded_projects`;
    - `capture.include_thinking` false→true;
    - `distill.provider` off→jev;
    - `distill.budget_share` 0.5→0.8.
    It names none of the reverse changes.
  - **`project_no_capture_parses_and_fails_closed`:** `[privacy]\nno_capture = true` parses. A broken file's
    `ProjectSettings::fail_closed()` has `no_capture: true`. Removing `no_capture` waits for the three-look gate
    (`LooseningGate` test helper).
  - **`capture_verdict_orders_wait_before_exclusion`** (`config.rs`): a degraded config gives `Wait`. With an excluded
    project and a valid config it gives `Excluded`. A `.alice.toml` with `no_capture = true` gives `Excluded`; a broken one
    gives `Wait`. Everything allowed gives `Capture`.
- [ ] **Step 2: Run** `… cargo test -p alice-core -p alice-daemon -j 4 capture_` and the names above. Expected: FAIL.
- [ ] **Step 3: Implement** the fields, the validation in the existing validate pass, and the entries in
  `loosened_protections`'s `checks`. The parse error message for project files now lists `no_capture`. The env-var mapping
  is automatic (`ALICE_CAPTURE_*`, `ALICE_DISTILL_*`). The `every_key_has_an_env_var…` test must still pass.
- [ ] **Step 4: Run** the full `alice-core` and `alice-daemon` unit tests (`… cargo test -p alice-core -p alice-daemon --lib -j 4`).
  Expected: PASS.
- [ ] **Step 5: Commit** `feat(sp2): capture and distill settings, project no_capture (Task 4)`.

---

## Task 5: Schema, origin, provenance and the capture writer ops

**Files:**
- Create: `crates/alice-core/src/store/sql/0006_capture.sql` (verbatim from spec §8.1, plus
  `CREATE INDEX capture_state_parent ON capture_state(parent_source_id)`)
- Create: `crates/alice-core/src/store/capture.rs`
- Modify: `crates/alice-core/src/store/mod.rs`, `crates/alice-core/src/model.rs`, `crates/alice-core/src/store/memories.rs`,
  `crates/alice-core/src/store/events.rs`, `crates/alice-core/src/store/import.rs` (writes `origin = 'import'`),
  `crates/alice-daemon/src/writer.rs`

**Interfaces:**
- Produces:
  ```rust
  // model.rs
  pub enum Origin { #[default] Agent, Distill, Import }      // as_str "agent" | "distill" | "import"
  pub struct Memory { /* … */ pub origin: Origin }          // #[serde(default)]
  // events.rs
  Actor::Distill(String)                                    // "distill:<provider>"; parse round-trips
  // memories.rs
  pub struct NewMemoryInput { /* … */ pub origin: Origin, pub provenance: Vec<String> /* chunk ids */ }
  // store/capture.rs
  pub struct CaptureState { pub source_id: String, pub uri: String, pub agent: String, pub session_id: String,
      pub parent_source_id: Option<String>, pub transcript_path: Option<String>, pub file_key: Option<String>,
      pub byte_offset: u64, pub next_ord: i64, pub last_event: Option<String>, pub last_capture_at: Option<String>,
      pub ended: bool, pub no_distill: bool, pub gaps: i64, pub distill_ord: i64, pub distill_state: DistillState,
      pub distill_attempts: i64, pub distill_next_try_at: Option<String>, pub project_id: String }
  pub enum DistillState { Idle, Due, Done, WaitingBudget, WaitingJudge, SkippedCloud, SkippedExcluded, SkippedBackfill, Failed }
  pub fn by_uri(conn: &Connection, uri: &str) -> Result<Option<CaptureState>>
  pub fn by_source(conn: &Connection, source_id: &str) -> Result<Option<CaptureState>>
  pub fn known_turns(conn: &Connection, agent: &str, keys: &[String]) -> Result<HashSet<String>>
  pub fn open_sources(conn: &Connection) -> Result<Vec<CaptureState>>                 // ended = 0
  pub fn due(conn: &Connection, now: &str, limit: usize) -> Result<Vec<CaptureState>> // 'due' states whose next try has come
  pub fn chunks_from(conn: &Connection, source_id: &str, ord: i64, limit: usize) -> Result<Vec<(String, i64, String)>> // id, ord, text
  pub fn captured_session_ids(conn: &Connection) -> Result<HashSet<String>>
  pub fn imported_session_end(conn: &Connection, session_id: &str) -> Result<Option<Option<String>>>
      // Some(ended_at) when a non-capture source with this session_id exists
  pub fn recent(conn: &Connection, limit: usize) -> Result<Vec<CaptureState>>
  // writer.rs
  WriteOp::CaptureSlice(Box<CaptureSlice>)
  pub struct CaptureSlice { pub source: SourceInput, pub project: ProjectChoice, pub init: CaptureInit,
      pub chunks: Vec<ChunkInput>, pub turn_keys: Vec<(String, i64)>, pub skipped_keys: Vec<String>,
      pub cursor: Option<(u64, i64)>, pub last_event: Option<String>, pub ended: bool, pub record_event: bool }
  pub struct CaptureInit { pub agent: String, pub session_id: String, pub parent_uri: Option<String>,
      pub transcript_path: Option<String>, pub file_key: Option<String>, pub no_distill: bool }
  WriteOutcome::Captured { source_id: String, inserted: usize }
  WriteOp::CaptureStateUpdate { source_id: String, change: CaptureChange }
  pub enum CaptureChange { Distill { state: DistillState, next_try_at: Option<String>, attempts: i64 },
      Distilled { distill_ord: i64 }, Ended, Gap { byte_offset: u64 }, FileKey { file_key: String, byte_offset: u64 } }
  ```

**`CaptureSlice` semantics, all in one transaction:**
- create the source if new, its project resolved by `resolve(project)`;
- an existing source keeps its `project_id` (spec §3.1);
- `capture_state` is created from `init` when absent;
- chunks are inserted with `INSERT OR IGNORE`, with `project_id` = the source's;
- `turn_keys` and `skipped_keys` go into `capture_turns` (`INSERT OR IGNORE`);
- `cursor` sets `byte_offset`/`next_ord`;
- `sources.ended_at` becomes the newest chunk's `authored_at`;
- one `source.capture` event when `record_event && inserted > 0`.

**`Remember`** with `origin = Distill`:
- the event actor is `Actor::Distill("jev")`;
- `provenance` rows are inserted in the insert's transaction (`INSERT OR IGNORE`);
- an idempotent duplicate inserts no provenance.

- [ ] **Step 1: Write the failing tests:**
  - **`migration_0006_adds_capture_tables_and_backfills_import_origin`** (`store/mod.rs` tests): a v5 database with one
    `judge_state = 'skipped'` memory and one normal one, migrated, has `origin` = `import` and `agent` respectively, and the
    two tables and four indexes exist.
  - **`capture_slice_writes_chunks_keys_and_cursor_atomically`** (`writer.rs` tests):
    - a slice with 3 chunks, 2 turn keys and a cursor `(4096, 3)` gives `Captured { inserted: 3 }`;
    - `known_turns` returns both keys;
    - `by_uri` shows `byte_offset == 4096` and `next_ord == 3`;
    - a second identical slice gives `inserted: 0` and leaves the cursor unchanged.
  - **`a_capture_source_keeps_its_first_project`:** a second slice naming another project stores its chunks under the
    first project.
  - **`skipped_keys_are_recorded_without_chunks`.**
  - **`a_distilled_remember_records_provenance_and_its_actor`:** a `Remember` with `origin: Distill` and
    `provenance: [c1, c2]` gives 2 `provenance` rows and an event actor of `distill:jev`. Repeating it with the same
    `idempotency_key` gives `Duplicate` and still 2 rows.
  - **`deleting_a_sources_chunks_cascades_its_state_but_keeps_turn_keys`:** after `delete_chunks` of all of a source's
    chunks, `by_uri` is `None` and `known_turns` still returns the keys.
- [ ] **Step 2: Run** `… cargo test -p alice-core -p alice-daemon -j 4 capture_slice migration_0006 distilled_remember cascades_its_state`.
  Expected: FAIL.
- [ ] **Step 3: Implement.** Add the migration file to the migration list (it needs a pre-migration backup, as every
  migration does). Write `store/capture.rs`. Add the writer arms in the existing `match` (a group commit like
  `Ingest`'s). The judge's `WriteOutcome` names list gains `"Captured"`.
- [ ] **Step 4: Run** the named tests, then `… cargo test --workspace -j 4` (callers of `NewMemoryInput`/`Memory`
  construct the new fields with `..Default::default()` or `origin: Origin::Agent`). Expected: PASS.
- [ ] **Step 5: Commit** `feat(sp2): capture tables, memory origin and provenance writes (Task 5)`.

---

## Task 6: The capture service (local notify, worker, sweep, status)

**Files:**
- Create: `crates/alice-daemon/src/capture.rs`
- Modify:
  - `crates/alice-daemon/src/service.rs`: field `capture: Arc<CaptureCtl>`, spawned in `Service::start` and joined in
    `shutdown`; the methods below; `Health.capture`.
  - `crates/alice-core/src/api.rs`: `CaptureNotify`, `CaptureStatus`, `CaptureSessionRow`, `CaptureHealth`.
  - `crates/alice-daemon/src/http.rs`: routes `POST /api/v1/capture/notify` and `GET /api/v1/capture/status`.
  - `crates/alice-daemon/src/spool.rs`: `SpoolEntry::CaptureNotify(CaptureNotify)` and its drain arm.
  - `crates/alice-daemon/src/service.rs` options: `capture_settle: Duration` (1.5 s), `capture_sweep_every: Duration`
    (60 s), `capture_quiet: Duration` (5 min), `capture_stale: Duration` (24 h). Tests shorten them.
- Test: `crates/alice-daemon/tests/capture_flow.rs`

**Interfaces:**
- Consumes: Tasks 2–5.
- Produces:
  ```rust
  // api.rs
  pub struct CaptureNotify { pub agent: String, pub event: String, pub session_id: String, pub transcript_path: String,
      pub cwd: Option<String>, pub agent_id: Option<String>, pub agent_transcript_path: Option<String>,
      pub source: Option<String>, pub reason: Option<String> }
  // event: "session-start" | "stop" | "subagent-stop" | "pre-compact" | "session-end" | "sweep" | "backfill"
  pub struct CaptureStatus { pub tenant: bool, pub enabled: bool, pub distill_provider: String, pub distill_state: String,
      pub sessions: Vec<CaptureSessionRow> }
  pub struct CaptureSessionRow { pub uri: String, pub project: String, pub chunks: u64, pub byte_offset: u64,
      pub last_capture_at: Option<String>, pub ended: bool, pub distill_state: String, pub gaps: i64 }
  pub struct CaptureHealth { pub sessions_24h: u64, pub chunks_24h: u64, pub last_capture_at: Option<String>,
      pub open_sessions: u64, pub distill_provider: String, pub distill_state: String, pub distill_due: u64,
      pub distilled_24h: u64, pub distill_dropped_24h: u64,
      #[serde(skip_serializing_if = "Option::is_none")] pub distill_warning: Option<String>,
      pub unrecognised_lines_7d: u64 }
  // Health gains `#[serde(default, skip_serializing_if = "Option::is_none")] pub capture: Option<CaptureHealth>`
  // service.rs
  impl Service {
      pub async fn capture_notify(&self, n: CaptureNotify) -> Result<()>     // validates, queues; Invalid in tenant mode
      pub async fn capture_status(&self) -> Result<CaptureStatus>
  }
  // capture.rs
  pub struct CaptureCtl;   // new(cfg) -> Arc<Self>, spawn(), notify(Job), stop()
  pub(crate) fn pass(ctx: &PassCtx, job: &Job) -> Result<PassReport>  // one read-render-verdict-write pass of one source
  ```

**A local pass (`pass`):**
1. Open the file read-only. It must be a regular file whose name ends in `.jsonl`. Check the stored `file_key`
   (`dev:inode:sha256(first line)`); a changed key or a shorter file resets the cursor to 0
   (`CaptureChange::FileKey`).
2. Read from `byte_offset`, at most `MAX_PASS_BYTES`. On `oversized_line`, stream forward to the next `\n` and set the
   cursor past it.
3. `read()` with `at_file_start = byte_offset == 0`. `close_trailing` is true for `stop`, `subagent-stop`, `pre-compact`
   and `session-end` after the settle delay, and for a sweep after `capture_quiet`.
4. Drop the turns whose key `known_turns` returns. Apply the MemPalace rule: turns with a `started_at` at or before an
   imported source's `ended_at` are skipped. That rule is wired in Task 18; until then it is a no-op hook.
5. Ask `capture_verdict` for the source's project:
   - `Excluded`: record the keys as `skipped_keys` and move the cursor;
   - `Wait`: store nothing.
6. Redact every chunk with `Redactors::for_root` of the source's project. In block mode a chunk with a secret becomes
   `[withheld: <detectors>]`.
7. Write `CaptureSlice`s of at most 50 chunks / 32 KiB. The turn keys and the cursor go with each turn's last slice, the
   tail chunks with the first slice. Pause 2 ms after each commit.
- Subagent passes (`agent_transcript_path`) use the subagent uri and `parent_uri`, and are dropped when the whole
  rendering is under 200 characters or `include_subagents` is off.
- `session-start` registers the source (no chunks needed).
- `session-end` marks it ended after a final pass.
- **The sweep** (every `capture_sweep_every`, and at start) passes open sources whose file grew or whose trailing turn has
  been quiet for `capture_quiet`. It marks ended, after a final pass, the sources idle for `capture_stale`.
- **In tenant mode** the worker never opens files. `capture_notify` returns
  `AliceError::Invalid("this server cannot read files on your machine; use the capture script (docs/capture.md)")`, and the
  sweep only marks stale sources ended.

- [ ] **Step 1: Write the failing tests** in `tests/capture_flow.rs`. A `Service::start` on a temp home (FakeEmbedder) with
  shortened options; transcripts are written into a temp dir as the test goes.
  - **`a_stop_notify_captures_the_completed_turn`:** write 2 turns, notify `stop`, wait until `capture_status` shows the
    source. `recall` with `include_raw` finds a word of turn 2, and the source's project is the `cwd`'s.
  - **`repeating_the_notify_stores_nothing_new`:** notify `stop` three times, then the chunk count is unchanged.
  - **`a_kill_between_slices_neither_loses_nor_doubles`:** a writer test hook (`ServiceOptions::capture_fail_after_slices: Option<usize>`,
    test-only) fails after slice 1 of a 3-slice turn. A second pass completes it. Chunk texts equal a clean capture's,
    exactly once each.
  - **`a_replaced_file_resets_and_skips_stored_turns`:** truncate and rewrite the file with turns 1–2 plus a new turn 3.
    Only turn 3 is stored.
  - **`a_continuation_file_skips_copied_turns`:** a new session id whose file starts with copies of turns 1–2 (same uuids).
    Only its new turn is stored.
  - **`subagent_files_are_their_own_sources`:** a `subagent-stop` with `agent_transcript_path` creates
    `claude-code:session:<s>:agent:<a>` with `parent_source_id` set. A 100-character subagent rendering is dropped.
  - **`an_excluded_project_moves_the_cursor_and_records_keys`:** a project in `capture.excluded_projects`: no chunks, the
    cursor at the end, the keys known. Removing the exclusion and notifying again stores nothing.
  - **`a_fail_closed_project_waits_and_keeps_the_cursor`:** a broken `.alice.toml` stores nothing and keeps
    `byte_offset` at 0. After the file is fixed and the sweep runs, the turns are stored.
  - **`block_mode_withholds_a_chunk_and_keeps_capturing`:** `privacy.redact_mode = "block"` and a turn holding
    `concat!("gh", "p_", …)` store a chunk `[withheld: github_token]`, and the next turn is stored normally.
  - **`the_trailing_turn_is_closed_by_the_quiet_sweep`.**
  - **`a_stale_session_is_ended_by_the_sweep`.**
  - **`twenty_sessions_at_once_capture_everything_once_beside_remembers`** (Review Focus 3): 20 transcript files of 5
    turns each, notified concurrently, while 50 `remember`s run. Every turn is stored once (count by key), every remember
    is stored, and the remember p95 is under 500 ms (a debug-build guard, not the budget).
  - **`a_forty_megabyte_transcript_is_captured_in_bounded_passes`** (Review Focus 4): a 40 MB file of 4,000 turns, notified
    once as `sweep`, ends with every key known after 5 or more passes, and no pass reads more than `MAX_PASS_BYTES`
    (`PassReport::bytes_read`).
  - **`a_spooled_notify_is_drained_into_a_pass`:** `spool::append` of a `CaptureNotify` followed by `spool::drain` captures
    the file.
  - **`the_tenant_daemon_refuses_notify_and_reads_no_files`:** a `--tenant` service answers `Invalid` with the message
    above.
- [ ] **Step 2: Run** `… cargo test -p alice-daemon -j 4 --test capture_flow`. Expected: FAIL.
- [ ] **Step 3: Implement `capture.rs`.**
  - One worker thread, `alice-capture`, lowered priority as maintenance does.
  - A `crossbeam_channel` queue of `Job { uri, notify }`, coalesced in a `HashMap<uri, Job>`, so the newest event wins and
    a `session-end` is never lost.
  - The settle delay is a per-job `not_before`.
  - Reads use the reader pool, writes `WriterHandle::submit_blocking`.
  - Also the service methods, the routes (`POST` notify → `202 {"queued": true}`, `GET` status), the spool variant, and
    `Health.capture`: counts from `capture_state` and `chunks.created_at`; the distill fields stay at their defaults until
    Task 12.
  - `unrecognised_lines_7d` is the sum of `ReadStats::unknown` per UTC day over the last 7 days, kept in memory by
    `CaptureCtl`. It restarts from zero with the daemon, which is acceptable for a format-drift hint.
- [ ] **Step 4: Run** the test file, then `… cargo test -p alice-daemon -j 4`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(sp2): daemon capture worker, notify and status (Task 6)`.

---

## Task 7: `alice hook` and the briefing route

**Files:**
- Create: `crates/alice/src/hook.rs`
- Modify:
  - `crates/alice/src/main.rs`: `Hook(cmd::HookArgs)`. `alice hook` is dispatched before the tracing subscriber and the
    full CLI runtime are set up.
  - `crates/alice/src/cmd.rs`.
  - `crates/alice-daemon/src/client.rs`: `DaemonClient::for_hook`, `capture_notify`, `capture_briefing`, `start_detached`.
  - `crates/alice-daemon/src/http.rs`: `GET /api/v1/capture/briefing`.
  - `crates/alice-daemon/src/service.rs`: `capture_briefing`.
  - `crates/alice-core/src/api.rs`: `BriefingQuery`.
- Test: `crates/alice/tests/cli.rs`

**Interfaces:**
- Produces:
  ```rust
  // api.rs
  pub struct BriefingQuery { pub source: Option<String>, pub cwd: Option<String>, pub project: Option<String> }
  // service.rs
  pub async fn capture_briefing(&self, q: BriefingQuery) -> Result<Option<String>>
      // None when capture.briefing is off or source is resume|fork; else the `context` text (≤ 10,000 chars) for the
      // project: from `cwd` locally, from `project` as a hint in tenant mode
  // http.rs: 200 text/plain; charset=utf-8 | 204
  // client.rs
  impl DaemonClient {
      pub async fn for_hook(home: &AliceHome, ping: Duration) -> Option<DaemonClient>   // no autostart
      pub async fn capture_notify(&self, n: &CaptureNotify, timeout: Duration) -> Result<()>
      pub async fn capture_briefing(&self, q: &BriefingQuery, timeout: Duration) -> Result<Option<String>>
  }
  pub fn start_detached(home: &AliceHome, exe: &Path) -> std::io::Result<()>   // spawn `daemon run`, do not wait
  // hook.rs
  pub const HOOK_DEADLINE: Duration = Duration::from_millis(90);
  pub const HOOK_PING: Duration = Duration::from_millis(30);
  pub const HOOK_REQUEST: Duration = Duration::from_millis(40);
  pub fn run(args: HookArgs) -> std::process::ExitCode        // always SUCCESS
  pub struct HookArgs { pub event: String, pub agent: String /* "claude-code" default */ }
  ```

**What `hook::run` does:**
1. Read stdin to EOF (at most 1 MiB).
2. Take `hook_event_name` (mapping `SessionStart`→`session-start`, `Stop`→`stop`, `SubagentStop`→`subagent-stop`,
   `PreCompact`→`pre-compact`, `SessionEnd`→`session-end`), else the argument.
3. Build a `CaptureNotify`.
4. `for_hook`, then `capture_notify`.
5. On `session-start` with `source` ∈ {`startup`, `clear`, `compact`} (or no `source`), and only while within
   `HOOK_DEADLINE`: `capture_briefing`, then print
   `{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":<briefing>}}`.
6. **When the daemon is unreachable:** `spool::append(SpoolEntry::CaptureNotify(..))`, then `start_detached` unless
   `ALICE_NO_AUTOSTART`.
7. **Errors** are appended to `logs/hook.log`: one line each, no transcript text, rotated to `hook.log.1` past 1 MB.
8. **Codex:** with `--agent codex`, nothing is printed on `session-start`.

- [ ] **Step 1: Write the failing tests** in `cli.rs` (they use `Env`):
  - **`hook_notifies_and_capture_follows`:** start the daemon (`alice status`), write a transcript under a temp
    `CLAUDE_CONFIG_DIR`, and pipe the Stop JSON into `alice hook stop --agent claude-code`. Exit 0 with empty stdout, and
    the raw tier eventually holds the turn.
  - **`hook_session_start_prints_the_briefing_json`:** after one `remember` with importance 5,
    `alice hook session-start` with `"source":"startup"` prints JSON whose `hookSpecificOutput.additionalContext` starts
    with `# Context · work`. With `"source":"resume"` it prints nothing. With `capture.briefing = false` it prints nothing.
  - **`hook_with_the_daemon_down_spools_and_exits_zero`:** with `ALICE_NO_AUTOSTART=1` and no daemon: exit 0, `spooled(&env) == 1`,
    and the entry's `op` is `capture_notify`. A later `alice daemon start` drains it, and the turn is captured.
  - **`hook_never_fails_on_garbage_input`:** stdin `not json`, empty stdin and 2 MiB of zeros each give exit 0, empty
    stdout, and one `hook.log` line.
  - **`hook_passes_a_thai_path_with_spaces_and_quotes`** (Review Focus 5): `cwd` = `<tmp>/งาน "x" y` with a git repo
    named `งาน-x`. The captured source's project slug is the slugified repo name, and the transcript path with spaces is
    read.
  - **`hook_finishes_under_100ms`** (`#[ignore]`; run in release): 200 runs of `alice hook stop` with the daemon up, and
    200 with it down (spool + detached start, `ALICE_NO_AUTOSTART=1` for the down case so no daemon storm). p95 < 100 ms
    each; the numbers are printed for `docs/benchmarks/sp2.md`.
  - **`the_briefing_route_answers_204_when_off`** (`http_api.rs`).
- [ ] **Step 2: Run** `… cargo test -p alice-memory -j 4 --test cli hook_`. Expected: FAIL.
- [ ] **Step 3: Implement** `hook.rs`, the client methods, the route and `capture_briefing`. The briefing reuses
  `Service::context` with `max_tokens` left at its default, and is cut to 10,000 characters at a line end.
- [ ] **Step 4: Run** the tests. Then run the latency test:
  `… cargo test --release -p alice-memory -j 4 --test cli hook_finishes_under_100ms -- --ignored --nocapture`.
  Expected: PASS, with both p95 under 100 ms. Note the numbers for Task 19.
- [ ] **Step 5: Commit** `feat(sp2): alice hook with spool, detached start and the SessionStart briefing (Task 7)`.

---

## Task 8: `alice hooks install | uninstall | status claude` and the doctor's capture line

**Files:**
- Create: `crates/alice-daemon/src/agent_hooks.rs`, `crates/alice-daemon/src/agent_hooks/json.rs`
- Modify:
  - `crates/alice/src/cmd.rs` and `crates/alice/src/main.rs`: `Hooks(cmd::HooksCmd)`.
  - `crates/alice-daemon/src/doctor.rs`: a no-daemon check named `capture hooks`.
- Test: `crates/alice/tests/cli.rs`, unit tests in `agent_hooks.rs`

**Interfaces:**
- Produces:
  ```rust
  pub enum HookAgent { Claude, Codex }
  pub struct HookTarget { pub agent: HookAgent, pub file: PathBuf, pub alice: PathBuf, pub alice_home: Option<PathBuf> }
  impl HookTarget { pub fn locate(agent: HookAgent, settings_override: Option<&Path>, env: &dyn Fn(&str) -> Option<OsString>) -> Result<Self> }
  pub struct HookChange { pub file: PathBuf, pub backup: Option<PathBuf>, pub added: Vec<String>, pub removed: usize,
      pub others: Vec<String> /* other hook commands mentioning "mempalace" or "alice" */, pub text: String }
  pub fn install(t: &HookTarget, dry_run: bool) -> Result<HookChange>
  pub fn uninstall(t: &HookTarget, dry_run: bool) -> Result<HookChange>
  pub fn status(t: &HookTarget) -> Result<HookStatus>   // installed: bool, binary_exists: bool, events: Vec<String>
  pub const CLAUDE_EVENTS: &[(&str, &str)] = &[("SessionStart","session-start"),("Stop","stop"),
      ("SubagentStop","subagent-stop"),("PreCompact","pre-compact"),("SessionEnd","session-end")];
  // json.rs
  pub enum JValue { Null, Bool(bool), Num(String), Str(String), Arr(Vec<JValue>), Obj(Vec<(String, JValue)>) }
  pub fn parse(text: &str) -> Result<JValue, String>
  pub fn print(v: &JValue) -> String       // 2-space indent, trailing newline
  ```
- **The command written:** `'<abs alice>' hook <event> --agent claude-code`, prefixed by `ALICE_HOME='<home>' ` when the
  home is not `~/.alice`, with single quotes escaped as `'\''`. Hooks are written with `"timeout": 10`.
- **Alice's entries** are those whose command's program file name is `alice` and whose words include
  `hook <event> --agent claude-code`. `HookTarget::locate` uses `--settings`, else `$CLAUDE_CONFIG_DIR/settings.json`, else
  `$HOME/.claude/settings.json`.
- **Backups:** `<file>.alice-backup-<yyyymmddThhmmssZ>`, mode 0600, newest 5 kept.

- [ ] **Step 1: Write the failing tests**, all with `HOME` and `CLAUDE_CONFIG_DIR` in a temp dir:
  - **`json_round_trips_and_keeps_key_order`:** parse and print of a settings file with nested objects, escapes,
    `é`, numbers like `1e3` and `-0.5`, and Thai text gives a value equal to `serde_json`'s. Key order is preserved.
  - **`install_creates_a_missing_settings_file`:** five events, each with one Alice hook.
  - **`install_merges_beside_other_hooks_and_keeps_them`:** an existing `Stop` hook running `afplay x.aiff` and a
    `PreToolUse` hook stay byte-identical inside the result. Top-level keys (`model`, `permissions`) keep their order.
  - **`install_is_idempotent`:** a second install is byte-identical to the first and makes no second backup.
  - **`install_repairs_hooks_of_a_moved_binary`:** entries naming `/old/path/alice hook stop --agent claude-code` are
    replaced, not duplicated.
  - **`uninstall_removes_only_alices_entries`:** removes exactly what install added, empty arrays and objects included,
    and leaves the other hooks.
  - **`a_malformed_settings_file_is_refused_untouched`:** a trailing comma gives an error naming the file. The bytes and
    mtime are unchanged, and no backup is made.
  - **`backups_are_private_and_capped_at_five`:** after 7 changing installs and uninstalls, 5 backups remain, each 0600.
  - **`install_lists_other_memory_hooks`:** an existing `SessionStart` running `~/.local/bin/alice-session-start` appears in
    `others`, and a `mempalace hook stop` appears too.
  - **`dry_run_changes_nothing`.**
  - **`doctor_warns_when_hooks_point_at_a_missing_binary`** (`doctor.rs`).
  - **`doctor_capture_line_warns_on_a_quiet_week_and_unrecognised_lines`** (`doctor.rs`, from a hand-built `Health`): the
    hooks are installed and `capture.last_capture_at` is 8 days old → Warn, "nothing was captured for 7 days". With
    `unrecognised_lines_7d > 0` → Warn, naming the count and "the agent's transcript format may have changed".
  - **`alice_hooks_cli_round_trip`** (`cli.rs`): `alice hooks install claude`, `status`, `uninstall`, with `--json`
    output shapes.
- [ ] **Step 2: Run** `… cargo test -p alice-daemon -p alice-memory -j 4 agent_hooks install_ uninstall_ alice_hooks`.
  Expected: FAIL.
- [ ] **Step 3: Implement** `json.rs` (a recursive-descent parser over `char_indices`, strict JSON) and `agent_hooks.rs`.
  The write is atomic: write a temp file in the same directory, `fsync`, rename. Before writing, check that
  `serde_json::from_str(&print(v))` equals the intended value converted to `serde_json::Value`; if not, refuse with
  "Alice could not rewrite <file> safely; it is unchanged". Codex arrives in Task 17: `HookAgent::Codex` returns
  `Invalid("Codex capture waits for a transcript sample")` until then.
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(sp2): alice hooks install/uninstall/status for Claude Code (Task 8)`.

---

## Task 9: Jev for distillation: the consumer, the ledger row and the budget share

**Files:**
- Modify:
  - `crates/alice-core/src/judge/mod.rs`: `Judge::ask_distill`.
  - `crates/alice-core/src/judge/jev.rs`: `JevCall.provider`, `JevStore::spent_by`, `JevLimits` per-consumer budget,
    `JevJudge::ask_distill`.
  - `crates/alice-daemon/src/judge_http.rs`: `DaemonJevStore::spent` sums `jev` and `jev:distill`; `spent_by`.
  - `crates/alice-daemon/src/writer.rs`: the ledger row is `call.provider`, and the cache provider stays `"jev"`.
  - `crates/alice-daemon/src/service.rs`: `Stats` sums both rows; `distill_cost_today_usd`.
  - `crates/alice-core/src/api.rs`: `Stats.distill_cost_today_usd`, `Stats.distilled_today`,
    `Stats.distill_dropped_today`.
  - `crates/alice-daemon/src/doctor.rs`: the 80 % budget warning uses the sum.

**Interfaces:**
- Produces:
  ```rust
  pub const LEDGER_JUDGE: &str = "jev";
  pub const LEDGER_DISTILL: &str = "jev:distill";
  trait Judge { fn ask_distill(&self, projects: &[&str], state: &str, questions: &[jev::Question], share: f64)
      -> JudgeResult<BTreeMap<String, Distribution>> { Err(JudgeError::Denied("distillation needs the Jev judge".into())) } }
  pub struct JevCall { /* … */ pub provider: String }
  trait JevStore { fn spent_by(&self, day: &str, provider: &str) -> Result<f64, String>; /* spent() = total */ }
  ```
  `JevJudge::ask_distill` works like `ask` with the configured retries and timeout. Its reservation requires:
  - `total_spent + est <= daily_budget`, and
  - `distill_spent + est <= share * daily_budget`.
  Breaking the second rule gives `JudgeError::Budget("the distillation share of today's Jev budget is used up …")`. The
  call is recorded with `provider = LEDGER_DISTILL`.

- [ ] **Step 1: Write the failing tests** (`jev.rs` tests with `Scripted` and `MemoryJevStore`; `service.rs` tests with
  `ScriptedJev`):
  - **`distill_calls_are_recorded_in_their_own_ledger_row`:** after one `ask_distill`, the ledger has a `jev:distill` row
    with 1 call, and `cost_today_usd` equals the sum of both rows.
  - **`distill_stops_at_its_share_while_the_judge_goes_on`:** budget $0.001, share 0.5, and a body estimated at about
    $0.0003. The second `ask_distill` succeeds, the third gives `Budget(..)` mentioning "distillation share", and a
    `compare` after that still runs.
  - **`the_judge_counts_distill_spend_toward_the_total`:** with distill spend at 90 % of the budget, a `compare` whose
    estimate passes the remainder gives `Budget`.
  - **`ask_distill_is_denied_for_the_heuristic`.**
  - **`ask_distill_passes_the_cloud_gate_and_redacts`:** a denied project gives `Denied` with no transport call. A body
    holding a secret is redacted in the sent body (`ScriptedJev::mentioned` is false for the secret).
- [ ] **Step 2: Run** `… cargo test -p alice-core -p alice-daemon -j 4 distill_calls distill_stops judge_counts ask_distill`.
  Expected: FAIL.
- [ ] **Step 3: Implement** as specified. Update the stats query at `service.rs` (the `provider = 'jev'` filter at about
  line 6986) and the `judge_store::spent` call sites to include both rows.
- [ ] **Step 4: Run** the tests, then `… cargo test -p alice-core -p alice-daemon -j 4` (the existing Jev budget tests must
  still pass). Expected: PASS.
- [ ] **Step 5: Commit** `feat(sp2): a distillation share of the Jev budget with its own ledger row (Task 9)`.

---

## Task 10: The distillation core (candidates, request, decisions)

**Files:**
- Create: `crates/alice-core/src/distill/mod.rs`
- Modify: `crates/alice-core/src/lib.rs`

**Interfaces:**
- Consumes: Task 3's `TAGS` and chunk format; `jev::Question`, `Choice`, `Distribution`.
- Produces:
  ```rust
  pub const MAX_CANDIDATES: usize = 40;      pub const STATE_CHARS: usize = 12_000;
  pub const PASSAGE_MIN: usize = 80;         pub const PASSAGE_MAX: usize = 1_200;
  pub const USER_MIN: usize = 20;            pub const USER_MAX: usize = 800;
  pub const TITLE_MAX: usize = 120;          pub const BODY_MAX: usize = 1_200;
  pub const RUN_MAX_TURNS: usize = 60;       pub const RUN_MAX_CHARS: usize = 120_000;
  pub const ALREADY_SAVED_MAX: usize = 20;   pub const OVERLAP: f64 = 0.6;
  pub struct RunChunk { pub id: String, pub ord: i64, pub text: String, pub subagent: bool }
  pub struct RunInput { pub uri: String, pub project: String, pub chunks: Vec<RunChunk>, pub closes_session: bool }
  pub enum CandidateSource { Summary, Conclusion, UserStatement, SubagentReport }
  pub struct Candidate { pub qid: String /* "c<n>" */, pub source: CandidateSource, pub text: String,
      pub prompt: Option<String>, pub chunk_ids: Vec<String>, pub handoff_ok: bool }
  pub struct Prepared { pub candidates: Vec<Candidate>, pub already_saved: Vec<String>, pub state: String,
      pub questions: Vec<Question>, pub consumed_ord: i64 /* last ord this run covers */ }
  pub fn prepare(input: &RunInput) -> Prepared
  pub struct DistillConfig { pub min_confidence: f64, pub max_per_run: usize }
  pub struct DistilledMemory { pub kind: Kind, pub to_global: bool, pub title: String, pub body: String,
      pub chunk_ids: Vec<String>, pub idempotency_key: String, pub p: f64 }
  pub fn decide(p: &Prepared, answers: &BTreeMap<String, Distribution>, uri: &str, cfg: &DistillConfig) -> Vec<DistilledMemory>
  pub const OPTIONS: &[(&str, &str)]   // the 11 options of spec §6.4, ids and descriptions verbatim
  ```
- **Option ids:** `decision`, `bugfix`, `howto`, `warning`, `preference_user`, `preference_project`, `fact`, `handoff`,
  `already_saved`, `ephemeral`, `unclear`. `handoff` is included only in the questions of candidates with
  `handoff_ok`: those from the last two turns of an input with `closes_session`.
- **The question instructions**, verbatim from spec §6.4: `Passage {i} from this session (the user had asked: «{prompt}»): «{text}». What lasting knowledge, if any, does it hold for future work?`.
  The parenthesis is left out when there is no prompt.
- **The idempotency key** is `format!("distill:{}", &hex(sha256(uri + "\x1f" + passage))[..32])`.
- **The body** is the passage, cut to `BODY_MAX` at a word boundary. For `Conclusion`/`SubagentReport` it gains a line
  `\n\nContext: {prompt}` when the prompt has 4 or more words.
- **The title** is the passage's first sentence, at most `TITLE_MAX` characters, cut at a word boundary.

- [ ] **Step 1: Write the failing tests** (in `distill/mod.rs`, over chunk text built with Task 3's renderer from
  fixtures):
  - **`candidates_come_in_priority_order`:** a run with a `[summary]`, two turns and a subagent report. The first
    candidates come from the summary's bullet groups, then the conclusions (last `[assistant]` block per turn), then the
    user statements, then the subagent report.
  - **`conclusions_are_split_into_passages_within_bounds`:** every `Conclusion` text is 80–1,200 characters, and a code
    block of more than 10 lines becomes `[code]`.
  - **`already_saved_lines_feed_the_state_and_filter_candidates`:** a `[tool alice remember] Use pnpm, not npm` line puts
    `Use pnpm, not npm` in `already_saved` and in the state under `Already saved in this session:`. A candidate repeating
    it is dropped before questions are built.
  - **`withheld_and_tool_only_text_is_never_a_candidate`.**
  - **`at_most_forty_questions_and_a_bounded_state`:** a 100-turn run has 40 questions and `state.chars().count() <= 12_000`.
  - **`handoff_is_offered_only_at_the_end_of_a_closing_run`.**
  - **`decide_keeps_confident_knowledge_up_to_the_cap`:** scripted answers, 12 of them `decision` at 0.9, give 8 memories
    with the highest p. `ephemeral`, `unclear` and `already_saved` answers give none. A `fact` at 0.65 is dropped (below
    0.7).
  - **`preference_user_goes_to_global_and_one_handoff_at_most`.**
  - **`bodies_titles_and_keys_follow_the_spec`:** a conclusion with the prompt "please fix the flaky lease test" gets a body
    ending with `Context: please fix the flaky lease test`. The title is at most 120 characters, cut at a word boundary.
    The key is `distill:` plus 32 hex characters and is stable across calls.
  - **`thai_passages_are_cut_at_character_boundaries`.**
- [ ] **Step 2: Run** `… cargo test -p alice-core -j 4 distill::`. Expected: FAIL.
- [ ] **Step 3: Implement `distill/mod.rs`** (pure; no I/O). Overlap is trigram Jaccard over lower-cased text, as
  `trigram_jaccard` in the test helpers computes it; reimplement it here, because test helpers are not product code.
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(sp2): extractive distillation candidates, Jev questions and decisions (Task 10)`.

---

## Task 11: The judge, search and briefing rules for distilled memories

**Files:**
- Modify:
  - `crates/alice-core/src/judge/mod.rs`: `JudgeMemory.origin`, `Relation.existing_origin`.
  - `crates/alice-core/src/judge/pipeline.rs`.
  - `crates/alice-daemon/src/judge_worker.rs`: fill origins, the stand-in rule.
  - `crates/alice-daemon/src/writer.rs`: new actions; stale targets of a distilled newcomer are dropped, never reviewed.
  - `crates/alice-daemon/src/maintenance.rs`: `merge_suggestions` skips `origin = 'distill'`.
  - `crates/alice-core/src/search/mod.rs` and `crates/alice-core/src/api.rs`: `Hit.origin`, hiding.
  - `crates/alice-core/src/context.rs`.

**Interfaces:**
- Produces:
  ```rust
  pub struct JudgeMemory { /* … */ pub origin: Origin }
  pub struct Relation { /* … */ pub existing_origin: Origin }
  pub enum JudgeAction { /* existing */ RelatesTo { other: String, p: f64 }, ReplaceDistilled { distilled: String, p: f64 } }
  pub struct Hit { /* … */ #[serde(default, skip_serializing_if = "Option::is_none")] pub origin: Option<String> } // "distill" only
  ```

**Rules in `decide`, for `new.origin == Distill`** (spec §6.6 table):
- duplicate ≥ auto → `DuplicateOf` (as now);
- duplicate between the thresholds → `DuplicateOf` (no review);
- updates ≥ auto → `Supersedes` only when `existing_origin == Distill`, else `RelatesTo`;
- updates between the thresholds → `RelatesTo`;
- contradicts ≥ review → `Contradicts`, with no review item;
- `review_only` (a stand-in) → nothing.

**Rule for `new.origin != Distill`:** a duplicate ≥ auto whose `existing_origin == Distill` gives
`ReplaceDistilled { distilled: existing }`. The writer then merges with the new memory as the survivor.

**Search:** a memory with `origin = Distill` whose `judge_state <> 'done'` is skipped in `Filters::score`. `get_memory`
still returns it. A distilled hit carries `origin: Some("distill")`, and its text-format line ends ` (auto)`.

**Briefing:**
- each section's SQL adds `AND NOT (origin = 'distill' AND judge_state <> 'done')`;
- distilled lines end ` (auto)`;
- `FALLBACK_KEY_MEMORIES` adds `AND origin <> 'distill'`;
- `RECENT_DECISIONS` orders `origin = 'distill', created_at DESC`.

- [ ] **Step 1: Write the failing tests:**
  - **`distilled_newcomers_never_create_review_items`** (`pipeline.rs`): one case per row of spec §6.6's table, with fixed
    distributions. `decide(..).reviews` is empty for every distilled case, and the actions are as listed above.
  - **`an_agent_memory_replaces_an_older_distilled_duplicate`** (`pipeline.rs` and a `service.rs` test with `ScriptedJev`
    at `duplicate` 0.95): the distilled memory is trashed with `duplicate_of` the new one, and the agent memory is active.
  - **`a_distilled_memory_never_supersedes_an_agent_memory`** (`service.rs`): updates 0.95, both active, a `relates_to`
    link, and no review.
  - **`a_stale_target_of_a_distilled_newcomer_is_dropped`** (`writer.rs`): the action is not applied, and no review is
    created.
  - **`the_stand_in_heuristic_leaves_distilled_memories_waiting`** (`judge_worker.rs`): with the Jev transport failing, a
    distilled memory stays `judge_state = 'retry'` with no review item.
  - **`unjudged_distilled_memories_are_hidden_from_recall_and_the_briefing`** (`service.rs`): pending → `recall` and
    `context` omit it and `get_memory` returns it; after the judge → both show it, the context line ending ` (auto)`.
  - **`merge_suggestions_skip_distilled_memories`** (`maintenance.rs`).
  - **`recent_decisions_list_agent_decisions_first`** (`context.rs`).
  - **`recall_json_marks_distilled_hits`** (`search/mod.rs` or `service.rs`).
- [ ] **Step 2: Run** `… cargo test -p alice-core -p alice-daemon -j 4 distilled_ agent_memory_replaces merge_suggestions_skip recent_decisions_list recall_json_marks`.
  Expected: FAIL.
- [ ] **Step 3: Implement.** `judge_worker` reads `origin` with each pending item and with each neighbour (the rows it
  already selects). `ReplaceDistilled` calls the existing `merge(conn, st, survivor = new, [distilled], actor, group)`.
- [ ] **Step 4: Run** the tests and the search-quality gate
  (`… cargo test -p alice-core -j 4 --test search_quality`). Expected: PASS.
- [ ] **Step 5: Commit** `feat(sp2): judge, search and briefing rules for distilled memories (Task 11)`.

---

## Task 12: The distill worker

**Files:**
- Create: `crates/alice-daemon/src/distill.rs`
- Modify:
  - `crates/alice-daemon/src/service.rs`: field `distill: Arc<DistillCtl>`; `capture_distill_now`; `Health.capture`
    distill fields; `Stats` counts.
  - `crates/alice-daemon/src/capture.rs`: a pass on `session-end`/`pre-compact`, or a quiet source, marks it `Due`.
  - `crates/alice-daemon/src/http.rs`: `POST /api/v1/capture/distill`.
  - `crates/alice-daemon/src/doctor.rs`: the `distill` line.
  - `crates/alice/src/cmd.rs` and `crates/alice/src/main.rs`: `Capture(cmd::CaptureCmd)` with `status` and
    `distill <session-id>`.
- Test: `crates/alice-daemon/tests/distill_flow.rs`

**Interfaces:**
- Consumes: Tasks 5, 9, 10 and 11.
- Produces:
  ```rust
  pub struct DistillCtl;  // new(cfg) -> Arc<Self>; spawn(); wake(); breaker_state() -> BreakerState; stop()
  pub struct Breaker { failures: u32, open_until: Option<Instant>, reopenings: u32 }
  pub const FAILURES_TO_OPEN: u32 = 3;
  pub const OPEN_FIRST: Duration = Duration::from_secs(15 * 60);
  pub const OPEN_MAX: Duration = Duration::from_secs(6 * 3600);
  impl Service { pub async fn capture_distill_now(&self, session_or_source: &str) -> Result<()> }
  pub(crate) fn write_memory(ctx: &DistillCtx, m: &DistilledMemory, src: &CaptureState) -> Result<InsertOutcome>
      // sync, on the worker thread: redact with Redactors::for_root(<source project root>), then
      // submit_blocking(WriteOp::Remember { input, project: ProjectChoice::Slug(<source slug> or "global") })
  ```

**One run:**
1. Load the source and its subagents' chunks from `distill_ord`, bounded by `RUN_MAX_TURNS`/`RUN_MAX_CHARS`.
2. Check the gate, in this order:
   - `distill.provider == Off` → `Idle`;
   - `judge.provider != jev` → `WaitingJudge`;
   - no Jev key → `WaitingJudge`, with the warning "no Jev key";
   - project excluded or `no_distill` → `SkippedExcluded`;
   - `!cloud_allowed` → `SkippedCloud`;
   - breaker open → keep `Due` with `next_try_at`.
3. `prepare`, then `judge.current().ask_distill(&[slug], &state, &questions, share)` on the worker thread.
4. `decide`.
5. For each memory, `write_memory`. It builds a `NewMemoryInput` with `kind`, title, body,
   `importance = distill.importance`, `origin: Distill`, `provenance`, `idempotency_key`, and
   `author = {agent: source agent, session: session id}`.
6. `CaptureChange::Distilled { distill_ord: consumed_ord + 1 }`, and the state becomes `Done`, or `Due` again if more
   chunks remain.

**Answers that are not failures:**
- `Budget` → `WaitingBudget` until the next UTC midnight;
- `Denied` → `SkippedCloud`.

**Failures** (`Unavailable`, `Failed`) count toward the breaker. The state goes to `Due` with a back-off of 5 minutes
doubling to 6 h, and `distill_attempts` increases. After 5 attempts the state is `Failed`.

`SkippedCloud` sources are re-checked every 5 minutes. Due sources are woken by the capture worker and by a 60 s timer.

**Health:** `distill_state` is `off`, `ready`, `waiting_judge`, `waiting_budget`, `breaker_open` or `no_key`;
`distill_due`, `distilled_24h` and `distill_dropped_24h` (distilled memories trashed in the last 24 h).

**The doctor's `distill` line:**
- `Ok` with "jev, N distilled today";
- `Warn` with its fix:
  - `waiting_judge`: `alice config set judge.provider jev`;
  - `no_key`: `alice config set-secret jev_api_key`;
  - `breaker_open`: "Jev failed 3 times; it is tried again at <time>";
  - `waiting_budget`: "today's distillation share is used up; raise distill.budget_share or judge.daily_budget_usd";
- `Ok` with "off" when the provider is off.

- [ ] **Step 1: Write the failing tests** in `tests/distill_flow.rs`. `Service::start` with
  `ServiceOptions { jev_transport: Some(ScriptedJev …) }`, `judge.provider = jev`, a test key and `distill.provider = jev`.
  The test file defines its own `DistillScript`, which implements the public `alice_core::judge::jev::JevTransport`. It
  parses each request body, answers every `c<n>` question from a `phrase → option` map (100 % on that option), counts its
  calls, and can be switched to answer 500 or to fail with an unavailable error. `ScriptedJev` is crate-private, so
  integration tests cannot use it.
  - **`an_ended_session_is_distilled_into_judged_memories`:** a captured session with a decision and a bugfix, then
    `session-end`. Two memories with `origin = distill`, their kinds, provenance to the right chunks, and importance 2.
    After the judge they are visible to `recall`.
  - **`distillation_waits_for_the_jev_judge`:** with `judge.provider = heuristic`, the source is `waiting_judge`, no
    transport call is made, and the doctor's `distill` line warns with the fix.
  - **`a_cloud_denied_project_is_never_sent`:** with `privacy.cloud_denied_projects = ["work"]`: zero transport calls and
    `skipped_cloud`.
  - **`three_failures_open_the_breaker`:** the transport answers 500 for distill. After 3 runs the health says
    `breaker_open`, the fourth source is not sent until `open_until`, and raw capture still works.
  - **`the_share_used_up_waits_for_tomorrow`.**
  - **`a_rerun_after_a_crash_stores_nothing_twice`:** run once, reset `distill_ord` to 0 (test hook), run again. The same
    memory count, the Jev cache was hit (no new transport call), and the idempotency keys returned duplicates.
  - **`a_memory_the_agent_saved_is_not_stored_again`:** the session holds `[tool alice remember] Use pnpm, not npm`, the
    agent memory exists, and the candidate is filtered (no question about it).
  - **`a_backfilled_session_is_not_distilled_unless_asked`** (state `skipped_backfill`; `alice capture distill <id>` makes
    it due).
  - **`the_quiet_trigger_distills_an_open_session`** (`distill.idle_minutes` shortened through a test option
    `distill_idle_override`).
- [ ] **Step 2: Run** `… cargo test -p alice-daemon -j 4 --test distill_flow`. Expected: FAIL.
- [ ] **Step 3: Implement** `distill.rs` and the wiring.
- [ ] **Step 4: Run** the tests, then `… cargo test -p alice-daemon -p alice-memory -j 4`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(sp2): background distillation with a breaker, budget share and doctor line (Task 12)`.

---

## Task 13: The synthetic labelled set, recorded answers and the evaluation

**Files:**
- Create: `crates/alice-daemon/tests/fixtures/distill/sessions/s01.jsonl … s30.jsonl`,
  `crates/alice-daemon/tests/fixtures/distill/labels.json`, `crates/alice-daemon/tests/fixtures/distill/recorded.jsonl`
- Create: `crates/alice-daemon/tests/distill_eval.rs`, `docs/benchmarks/sp2-distill.md`

**Interfaces:**
- Consumes: Task 12 (the whole pipeline through `Service`).
- **`labels.json` shape:**
  ```json
  {"sessions": [{"file": "s01.jsonl", "kind": "gold",
     "gold": [{"phrase": "...", "kinds": ["bugfix","howto"]}]}]}
  ```
  `kind` is `gold`, `nothing` or `already`.
- **`recorded.jsonl`:** one line per Jev request, `{"request_body": …, "response_body": …}`, bodies only.

- [ ] **Step 1: Write the set.** 30 sessions in the shape of Task 1's fixtures:
  - **20 `gold` sessions** with 1–4 items each (about 50 in all);
  - **6 `nothing` sessions** (chit-chat, progress narration, plans for the next minutes);
  - **4 `already` sessions**, where every gold item is also `remember`ed through a `[tool alice remember]` call and an
    existing agent memory.
  About 10 sessions are in Thai or mixed Thai and English. Hand-written from scratch, with no owner data and no
  secret-shaped strings.
- [ ] **Step 2: Write `distill_eval.rs`.**
  - **The replay test, `distill_eval_replays_the_recorded_answers`:** a transport that answers from `recorded.jsonl` by
    the request body's SHA-256. It fails on any request it has no answer for, so the pipeline is pinned. It computes the
    metrics of spec §11.3 and asserts the declared thresholds:
    ```rust
    assert!(m.precision >= 0.80, "{m:?}");
    assert!(m.recall >= 0.60, "{m:?}");
    assert!(m.doubles <= m.already_items / 10, "{m:?}");
    assert!(m.flood <= 2, "{m:?}");
    ```
  - **`distill_eval_live`** (`#[ignore]`): the real Jev, through `ReqwestTransport` and the key from `TYPESAFE_API_KEY`.
    It writes `recorded.jsonl` (bodies only) and prints the metrics and spend. It stops if the spend passes $0.10.
- [ ] **Step 3: Run the live evaluation once.** `TYPESAFE_API_KEY` is already exported in this shell. Run this one command
  **without** `env -u TYPESAFE_API_KEY`, and never source or read `.env`:
  `… nice -n 15 cargo test -p alice-daemon -j 4 --test distill_eval distill_eval_live -- --ignored --nocapture`
  Then check the fixture: `grep -c -F -- "$TYPESAFE_API_KEY" crates/alice-daemon/tests/fixtures/distill/recorded.jsonl`
  must print `0`.
- [ ] **Step 4: If a threshold fails,** make one round of changes to the candidate rules, the option wording or
  `distill.min_confidence` (never the set or the thresholds). Re-run once, record both runs in `sp2-distill.md`, and report
  to the controller before going on.
- [ ] **Step 5: Run the replay test** `… cargo test -p alice-daemon -j 4 --test distill_eval`. Expected: PASS.
- [ ] **Step 6: Write `docs/benchmarks/sp2-distill.md`:** the set, the metrics per kind and language, the spend, the
  caveats (one author's labels, a synthetic set), and both runs if Step 4 applied.
- [ ] **Step 7: Commit** `test(sp2): synthetic distillation set, recorded Jev answers and evaluation (Task 13)`.

---

## Task 14: The tenant transcript route, project hints and adoption

**Files:**
- Modify:
  - `crates/alice-daemon/src/capture.rs`: `upload(ctx, query, bytes)`.
  - `crates/alice-daemon/src/service.rs`: `capture_transcript`.
  - `crates/alice-daemon/src/http.rs`: `POST /api/v1/capture/transcript`, with
    `DefaultBodyLimit::max(TRANSCRIPT_BODY_LIMIT)` on that route.
  - `crates/alice-core/src/api.rs`: `TranscriptQuery`, `TranscriptAnswer`.
- Test: `crates/alice-daemon/tests/capture_tenant.rs` (uses `support::caddy::Server`)

**Interfaces:**
- Produces:
  ```rust
  pub const TRANSCRIPT_BODY_LIMIT: usize = 8 << 20;
  pub struct TranscriptQuery { pub agent: String, pub session: String, pub subagent: Option<String>, pub offset: u64,
      pub event: String, pub project: Option<String>, pub cwd: Option<String>,
      pub no_distill: Option<u8>, pub no_capture: Option<u8> }
  pub struct TranscriptAnswer { pub next_offset: u64, pub turns_stored: u64, pub turns_skipped: u64,
      pub chunks_stored: u64, pub oversized_line: bool }
  impl Service { pub async fn capture_transcript(&self, q: TranscriptQuery, body: Bytes) -> Result<Result<TranscriptAnswer, u64 /* 409 cursor */>> }
  ```
- **The cursor:** a `q.offset` lower than the cursor gives `Err(cursor)`, which the route answers as
  `409 {"next_offset": cursor}`. A higher offset is a gap (`CaptureChange::Gap`).
- **The project:** in tenant mode it comes from `project` as the in-process `project_hint` (P8); locally from `cwd`.
- **The pipeline:** the same as a local pass from Step 3 on, with `at_file_start = q.offset == 0` and
  `close_trailing = q.event == "session-end"`.
- **Flags:** `no_capture=1` makes the request a no-op `200` with `next_offset = offset + body length up to its last \n`.
  `no_distill=1` sets `capture_state.no_distill`.
- **Adoption:** in tenant mode `transcript_path` and `file_key` are ignored.

- [ ] **Step 1: Write the failing tests** (`capture_tenant.rs`, through the Caddy emulator, `reqwest` with the bearer
  token):
  - **`an_upload_captures_complete_turns_and_returns_the_cursor`:** post bytes holding 2 complete turns and half a third.
    `next_offset` is the third prompt's line offset, and `recall` on the tenant finds turn 2.
  - **`a_lower_offset_gets_409_with_the_cursor`:**
    ```rust
    assert_eq!(resp.status(), 409);
    assert_eq!(body["next_offset"], cursor);
    ```
  - **`a_resend_from_zero_stores_nothing_twice`:** with the client state lost, resending everything from 0 gives a `409`.
    Following it stores no new chunk.
  - **`a_forward_gap_is_accepted_and_counted`.**
  - **`the_server_redacts_and_never_reads_paths`:** a body holding `concat!("sk-", "ant-…")` is stored redacted. A query
    `cwd=/etc` is ignored in tenant mode.
  - **`the_project_hint_shares_the_mcp_placeholder`:** `project=Repo One` creates `import:repo-one`, and a later MCP
    `remember` from a root named `Repo One` lands in the same project.
  - **`no_capture_and_no_distill_flags_are_honoured`.**
  - **`the_body_limit_is_eight_mebibytes`:** a 9 MiB body gives `413`.
  - **`an_adopted_database_is_not_captured_twice`** (spec §9.4): capture a transcript with a local `Service` on home H.
    Stop it. Start a `--tenant` service on H through the emulator. Upload the same file from offset 0: a `409` jump, no
    chunk added. Append a new turn and upload from the jump: exactly one new turn. The adopted row's `transcript_path` is
    never opened (the path is removed before the tenant starts and nothing fails).
  - **`the_tenant_briefing_resolves_the_hint`:** `GET /api/v1/capture/briefing?project=Repo%20One&source=startup` answers
    the context text of `repo-one`.
- [ ] **Step 2: Run** `… cargo test -p alice-daemon -j 4 --test capture_tenant`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests and `--test server_mode` (unchanged). Expected: PASS.
- [ ] **Step 5: Commit** `feat(sp2): tenant transcript uploads with an offset protocol, hints and adoption (Task 14)`.

---

## Task 15: The tenant scripts (`hook.sh`, `setup.sh`) and their public routes

**Files:**
- Create: `crates/alice-daemon/scripts/capture/hook.sh`, `crates/alice-daemon/scripts/capture/setup.sh`
- Modify: `crates/alice-daemon/src/http.rs`: public routes `GET /api/v1/capture/setup.sh` and
  `GET /api/v1/capture/hook.sh`, serving `include_str!` with `text/plain; charset=utf-8` and `Cache-Control: no-store`.
  They sit in the `public` router, behind `host_guard`.
- Test: `crates/alice-daemon/tests/capture_scripts.rs` (`#[cfg(unix)]`)

**Interfaces:**
- **`hook.sh`** subcommands: `hook <agent>` (reads stdin), `run <agent> <event> <session> <path> <cwd> [<agent_id> <agent_path>]`
  (the background worker), `status` and `uninstall`.
- **Files it uses, under `~/.config/alice/`:**
  - `token` and `url` (0600, one line each; installed by the team's setup document or by `setup.sh`; read with `head -n 1`, never written by `hook.sh`);
  - `capture.conf` (only `briefing=on|off`; no secret);
  - `state/<agent>-<session>[-<agent_id>].offset` (content: `<offset> <path>`);
  - `capture.log` (≤ 256 KB, no token, no transcript text);
  - `paused-until` (a 401 back-off of 1 hour).
- **`setup.sh <url>`:**
  - uses `~/.config/alice/url` when it exists (the argument may differ: say so, keep the file), else writes the argument
    there (0600, directory 0700);
  - uses `~/.config/alice/token` when it exists and `GET <url>/api/v1/capture/status` accepts it, and never prompts then;
    it asks only when the file is missing or the token is refused, from `/dev/tty`, or from stdin when
    `ALICE_SETUP_TOKEN_STDIN=1` (tests), and writes the answer to that same file (0600). The token is never in argv or
    the environment, and there is no second copy;
  - writes `capture.conf` (`briefing=on`) and the hook script;
  - writes the marketplace `~/.config/alice/claude-marketplace/.claude-plugin/marketplace.json` (`{"name":"alice-local",…}`)
    and the plugin `alice-capture/` (`.claude-plugin/plugin.json` with the tenant's version, and `hooks/hooks.json` with
    the five events running `sh "$HOME/.config/alice/alice-capture.sh" hook claude-code`);
  - runs `claude plugin marketplace add …` and `claude plugin install alice-capture@alice-local --scope user`, or prints
    the two `/plugin` commands when `claude` is missing or fails;
  - Codex: per P10;
  - prints the summary, including "distillation: on/off" from `/capture/status`.
- **Every curl call:**
  `printf 'header = "Authorization: Bearer %s"\n' "$token" | curl -K - -fsS --connect-timeout 5 --max-time 30 …`
  The briefing call uses `--connect-timeout 1 --max-time 2`.

- [ ] **Step 1: Write the failing tests** in `capture_scripts.rs`. Each runs a real `sh` with `HOME` set to a temp dir,
  `PATH` = a temp `bin/` holding a fake `claude` (a script that appends `"$@"` to `$HOME/claude-calls`) plus `/usr/bin:/bin`,
  and the Caddy emulator's `url("ann")`.
  - **`setup_installs_conf_script_and_plugin`:**
    - `token` and `url` are mode 0600 in a 0700 directory, and `capture.conf` holds only `briefing=on`;
    - `alice-capture.sh` is 0700;
    - `marketplace.json`, `plugin.json` and `hooks.json` parse as JSON;
    - `hooks.json` has the five events;
    - `claude-calls` holds `plugin marketplace add` and `plugin install alice-capture@alice-local --scope user`.
  - **`setup_reuses_an_existing_token_file_without_prompting`:** with `token` and `url` pre-written (as the team's setup
    document leaves them) and no stdin, setup completes, never prompts, and leaves both files byte-identical.
  - **`setup_prompts_only_when_the_token_file_is_missing_or_refused`:** a missing file → one prompt, the answer written to
    `token`; a file holding a refused token → one prompt, the file replaced.
  - **`setup_rejects_a_wrong_token_and_writes_nothing`** (a wrong token typed at the prompt leaves no `token` file and no
    plugin).
  - **`the_token_never_appears_in_argv_or_logs`:** a sampler thread runs `ps -axww -o args=` every 5 ms during setup and
    three hook runs, and asserts the token string is never in its output. It is never in `capture.log` or
    `capture.conf`, and it is in `~/.config/alice/token` only.
  - **`a_transcript_grown_in_steps_is_captured_once`:** three hook `stop` runs between appends, then (after the background
    jobs finish, polled on the state file) the tenant holds every turn exactly once.
  - **`a_lost_state_file_resyncs_with_one_409`.**
  - **`the_server_down_keeps_the_offset_and_session_start_sweeps`:** pause the emulator, run `stop`, unpause, run
    `session-start` of another session. The first session is finished.
  - **`session_start_prints_the_briefing_as_plain_text`:** stdout holds `# Context · `.
  - **`the_briefing_gives_up_within_two_seconds`:** `Server::pause(true)` makes the emulator hold connections. The hook's
    wall time is under 2.5 s, stdout is empty and the exit status is 0.
  - **`briefing_off_prints_nothing`** (both the conf line and `capture.briefing = false`).
  - **`no_capture_in_alice_toml_skips_and_cloud_denied_sends_no_distill`.**
  - **`hook_script_handles_utf8_and_skips_u_escapes`** (Review Focus 5): a Thai `cwd` and transcript path with spaces work.
    A `transcript_path` holding `ก` is skipped with one log line.
  - **`uninstall_removes_what_setup_wrote`:** `claude-calls` holds `plugin uninstall`, and the marketplace is removed.
    After the `yes` answer (`ALICE_SETUP_ASSUME_YES=1` in tests) the script, `state/`, `capture.conf`, `capture.log` and
    `paused-until` are gone, while `token` and `url` are untouched (they belong to the MCP setup).
  - **`the_public_scripts_need_no_token_and_hold_no_secret`** (`http_api.rs`): `GET` without a token gives 200. The bodies
    match `include_str!`, and `grep -c -F -- "$TYPESAFE_API_KEY"` on them gives 0.
- [ ] **Step 2: Run** `… cargo test -p alice-daemon -j 4 --test capture_scripts`. Expected: FAIL.
- [ ] **Step 3: Write the scripts.** They are POSIX `sh` (checked with `sh -n`, plus `dash -n` where `dash` exists), start
  with `set -u` and never `set -x`. The URL check is `https://`, or `http://` to `127.0.0.1`/`localhost`. JSON fields are
  extracted with the `sed` pattern of spec §5.3 step 1. Background work runs with `nohup sh "$0" run … >/dev/null 2>&1 </dev/null &`.
- [ ] **Step 4: Run** the tests on macOS here, and on ubuntu through CI after the push. Expected: PASS on both.
- [ ] **Step 5: Commit** `feat(sp2): tenant capture scripts and the local Claude Code plugin marketplace (Task 15)`.

---

## Task 16: The capture-scoped retention job

**Files:**
- Modify:
  - `crates/alice-daemon/src/maintenance.rs`: `Job::CaptureRetention`, named `capture_retention`, daily.
  - `crates/alice-core/src/store/maintenance.rs`.

**Interfaces:**
- Produces:
  ```rust
  pub fn capture_chunks_older_than(conn: &Connection, cutoff: &str, limit: usize) -> Result<Vec<i64>>
  pub fn capture_raw_bytes(conn: &Connection) -> Result<u64>
  pub fn oldest_capture_chunks(conn: &Connection, limit: usize) -> Result<Vec<(i64, u64)>>
  ```
  Each joins `chunks.source_id = capture_state.source_id`.
- The job mirrors `Job::RawRetention`'s step on `settings.capture.raw_retention`: slices of `SLICE`, the
  `WriteOp::DeleteChunks`, and one `raw.retention` event per run with `reason = "capture: <retention>"`.

- [ ] **Step 1: Write the failing tests** (`maintenance.rs`):
  - **`capture_retention_deletes_only_captured_chunks`:** an imported source and a capture source, both with chunks 200
    days old, and `capture.raw_retention = "180d"`. After `run_job(Job::CaptureRetention, now, true)` the capture chunks
    are gone and the imported ones remain. `storage.raw_retention = forever` leaves everything else alone.
  - **`capture_retention_keeps_cursor_and_keys_of_a_live_source`:** old chunks of an open source are deleted, while its
    `capture_state` (new chunks remain) and turn keys stay.
  - **`capture_retention_by_size`.**
  - **`the_env_pin_wins_over_a_patch`:** `ALICE_CAPTURE_RAW_RETENTION=180d` with a `PATCH capture.raw_retention = forever`
    still deletes, and the patch answer names the environment variable.
- [ ] **Step 2: Run** `… cargo test -p alice-core -p alice-daemon -j 4 capture_retention the_env_pin_wins`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(sp2): capture-scoped raw retention (Task 16)`.

---

## Task 17: Codex (the reader and installer, or only the seam; P10)

**Files:**
- Create (only with a sample): `crates/alice-core/src/capture/codex.rs`, fixtures `crates/alice-core/tests/fixtures/capture/codex/*.jsonl`
- Modify: `crates/alice-core/src/capture/mod.rs`, `crates/alice-daemon/src/agent_hooks.rs` (Codex `hooks.json`),
  `crates/alice-daemon/src/capture.rs` (the null-path search), `crates/alice-daemon/scripts/capture/setup.sh` (the Codex
  part)

**Interfaces:**
- `read(AgentKind::Codex, …)` follows the same contract as Claude Code.
- **Turn key:** `turn_key(None, timestamp, first_text)` unless the rollout carries ids (per `FORMAT.md`).
- **Null-path search:** `fn find_rollout(codex_home: &Path, session_id: &str) -> Option<PathBuf>` searches the newest 7
  `sessions/YYYY/MM/DD` folders for `rollout-*<session_id>*.jsonl`. `.zst` files are counted, not read (R21).
- **`alice hooks install codex`** writes `$CODEX_HOME/hooks.json` (else `~/.codex/hooks.json`) with the five events and
  `'<abs alice>' hook <event> --agent codex`. It refuses when `config.toml` has a `[hooks]` table or `[[hooks.*]]`
  (detected with `toml_edit`), and ends with the `/hooks` trust reminder.

- [ ] **Step 1: Write the failing tests.**
  - **With a sample:**
    - `codex_turns_and_rendering_follow_the_contract`;
    - `codex_samples_read_without_unknown_lines`;
    - `a_null_codex_path_is_found_by_session_id`;
    - `install_codex_writes_hooks_json_and_reminds_to_trust`;
    - `install_codex_refuses_inline_config_hooks`;
    - `a_zst_rollout_is_counted_not_read`;
    - the tenant setup writes `hooks.json` when it is absent and prints the entries otherwise
      (`setup_handles_codex_hooks_json`).
  - **Without a sample:** `install_codex_waits_for_a_sample` (the refusal text) and
    `the_codex_seam_exists_and_reads_nothing`.
- [ ] **Step 2: Run** `… cargo test -p alice-core -p alice-daemon -j 4 codex`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests. Expected: PASS.
- [ ] **Step 5: Commit** `feat(sp2): Codex capture` or `feat(sp2): Codex capture seam (no sample yet)`, with `(Task 17)`.

---

## Task 18: MemPalace migration and backfill

**Files:**
- Modify:
  - `crates/alice-daemon/src/capture.rs`: the continue-after-import rule of spec §9.2, at the Task 6 hook.
  - `crates/alice-core/src/import/plan.rs`: `Planner.captured_sessions`.
  - `crates/alice-daemon/src/importer.rs`: fills it from `captured_session_ids`.
  - `crates/alice/src/cmd.rs`: `alice capture backfill`.
- Test: `crates/alice-daemon/tests/capture_flow.rs`, `crates/alice-daemon/tests/import_flow.rs`

**Interfaces:**
- **`Planner`:** `pub struct Planner<'a> { pub mapper: Mapper<'a>, pub survey: &'a Survey, pub captured_sessions: &'a HashSet<String> }`.
  A `RawSession` drawer whose file stem is in the set gives `Planned::Skipped { reason: "captured by alice capture" }`,
  counted in `drawers_accounted`.
- **The first capture of session S:** `imported_session_end(conn, S)`:
  - `Some(Some(t))` → skip turns with `started_at <= t`; a turn with no `started_at` is kept;
  - `Some(None)` → keep everything.
- **Backfill:** `alice capture backfill <claude|codex> [--since YYYY-MM-DD] [--project slug] [--distill]`.
  - It enumerates `<CLAUDE_CONFIG_DIR or ~/.claude>/projects/*/*.jsonl` (and `subagents/`), filters by mtime `--since`,
    and sends one `CaptureNotify { event: "backfill" }` per file.
  - The daemon treats `backfill` as a sweep with `close_trailing = true`. The new sources get `skipped_backfill`, unless
    `--distill`, which makes them `Due`.
  - It prints the files queued.

- [ ] **Step 1: Write the failing tests:**
  - **`a_session_mempalace_mined_continues_after_the_import`:** an imported source (`file://…/S.jsonl`, `session_id` S,
    `ended_at` T). Capturing S's file stores only turns after T.
  - **`an_imported_source_without_an_end_time_captures_everything`.**
  - **`the_importer_skips_sessions_capture_holds`** (`import_flow.rs`, the fixture palace with a transcript drawer for
    session S, after capturing S): the report counts it as skipped, `drawers_seen == drawers_accounted`, and no
    `file://` source for S is created.
  - **`backfill_queues_files_since_a_date_and_does_not_distill`** (`cli.rs`, throwaway HOME): 3 files, one older than
    `--since`. 2 sources are captured with `skipped_backfill`. With `--distill` they are due.
- [ ] **Step 2: Run** `… cargo test -p alice-core -p alice-daemon -p alice-memory -j 4 mempalace_mined imported_source_without importer_skips backfill_queues`.
  Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the tests and `--test import_flow` whole. Expected: PASS.
- [ ] **Step 5: Commit** `feat(sp2): MemPalace cut-over rules and backfill (Task 18)`.

---

## Task 19: Docs

**Files:**
- Create: `docs/capture.md`, `docs/benchmarks/sp2.md`
- Modify: `README.md`, `README.th.md` (same commit), `docs/server.md`

- [ ] **Step 1: Write `docs/capture.md`:**
  - what is captured, and what never is (the rendering table in short);
  - local install and removal (`alice hooks install|uninstall claude`, Codex and `/hooks` trust);
  - privacy (`no_capture`, `capture.excluded_projects`, cloud denial, block mode);
  - distillation (off by default; how to turn it on; what `(auto)` means; that it never asks you);
  - settings (the §7 table);
  - backfill;
  - the MemPalace cut-over (spec §9.1, verbatim steps);
  - troubleshooting from `alice doctor`.
- [ ] **Step 2: Write `docs/server.md`, "Capture for team members":**
  - the setup line, and what it installs (the local marketplace plugin);
  - the bounded briefing and how to turn it off;
  - the privacy difference on this path (no `.alice.toml` mirroring; the two flags);
  - the 180-day capture retention pinned by `ALICE_CAPTURE_RAW_RETENTION`;
  - Jev on with the owner's shared key and the deployment-wide ceiling ($5.00/day at the default);
  - uninstall;
  - the interfaces pane 3 implements (spec §9.5).
  No server address anywhere.
- [ ] **Step 3: Update `README.md` and `README.th.md`:**
  - the SP2 row becomes "built (alpha)";
  - a short "Automatic capture" section pointing at `docs/capture.md`;
  - the CLI list gains `hook`, `hooks` and `capture`.
- [ ] **Step 4: Write `docs/benchmarks/sp2.md`:** the hook latency numbers from Task 7 (both cases, p50/p95/max, machine
  and load), the capture throughput from the 40 MB test, and a placeholder section for Task 20's real-session numbers.
- [ ] **Step 5: Check** `grep -n -E '([0-9]{1,3}\.){3}[0-9]{1,3}' docs/capture.md docs/server.md docs/benchmarks/sp2.md README.md README.th.md`.
  Only loopback and documentation addresses may appear. Also check that `grep -c -F -- "$TYPESAFE_API_KEY"` on each file
  is 0.
- [ ] **Step 6: Commit** `docs(sp2): capture guide, server section, READMEs and hook benchmarks (Task 19)`.

---

## Task 20: The owner's real sessions, then the gates

**Files:**
- Modify: `docs/benchmarks/sp2.md` (numbers only)
- Ledger: the real-session report stays in `$S/real/`, never committed

- [ ] **Step 1: Choose sessions with the controller.** Copy 3–5 Claude Code sessions read-only into `$S/real/` (the
  controller's conditions; spec §11.4). Use `ALICE_HOME=$S/real/home` and `ALICE_EMBEDDER=fake` (or the linked real
  model).
- [ ] **Step 2: Capture them** with `alice capture backfill claude` against `CLAUDE_CONFIG_DIR=$S/real/claude`.
  - Start that home's daemon from this shell. `TYPESAFE_API_KEY` is already exported here, and the judge reads it from
    the environment. Never source or read `.env`, and never pass the key as an argument.
  - Then `alice config set judge.provider jev` and `alice config set distill.provider jev`.
  - Run `alice capture distill <id>` for each session.
- [ ] **Step 3: Write `$S/real/report.md`:** per session the chunks, candidates, kept memories with kind and p,
  duplicates the judge caught, and the cost. The controller and the owner read it there.
- [ ] **Step 4: Record in `docs/benchmarks/sp2.md`** only the aggregates: counts per session, chunks per day extrapolated,
  spend, and the latency of capture passes. No text.
- [ ] **Step 5: Delete the copies** with `rm -rf "${S:?}/real/claude"` (the report stays in scratch). Record that in the
  ledger.
- [ ] **Step 6: Run the gates:**
  - `cargo fmt --all --check`;
  - `cargo clippy --workspace --all-targets -- -D warnings`;
  - `env -u TYPESAFE_API_KEY -u ALICE_EMBEDDING_API_KEY nice -n 15 cargo test --workspace -j 4`;
  - the release latency test of Task 7;
  - `pgrep -fl "alice daemon run"`, which must show nothing the run started.
  Push, and check that CI is green on ubuntu and macOS.
- [ ] **Step 7: Commit** `docs(sp2): real-session check aggregates (Task 20)`, then hand over for the final whole-branch
  review.

---

## Re-order (controller priority, 2026-10-16)

The owner is now a server tenant, and Alice must replace MemPalace soon. The tenant path comes first.

**New order:** 1–5 (done or in progress) → **6a → 14 → 15** → 9 → 10 → 11 → 12 → 13 → 16 → **6b** → 7 → 8 → 17 → 18 → 19
→ 20.

Task 6 is split, and parts of Tasks 7 and 14 move.

### Task 6a: The capture pipeline and the transcript service method (from Tasks 6 and 14)

**Files:**
- `crates/alice-daemon/src/capture.rs`: the pipeline only, `CaptureCtl::apply(input) -> PassReport`.
- `crates/alice-daemon/src/service.rs`: `capture_transcript`, `capture_status`, `Health.capture`.
- `crates/alice-core/src/api.rs`: `TranscriptQuery`, `TranscriptAnswer`, `CaptureStatus`, `CaptureSessionRow`, `CaptureHealth`.
- `crates/alice-daemon/tests/capture_flow.rs`.

**The pipeline** is Task 6's pass steps 3–7, run on given bytes, with the ledger's rulings:
- tails keyed `tail:<key>`;
- `force_close` for session-end and stale-end, `close_trailing` for stop/subagent-stop/pre-compact/quiet;
- a withheld chunk keeps its tag;
- `capture_verdict` is asked for every identity of the project;
- `keys_new` is checked;
- the last storing slice is flagged for the event;
- the MemPalace rule is a no-op hook until Task 18;
- in tenant mode at service start, `UPDATE capture_state SET transcript_path = NULL, file_key = NULL`.

**The service method** is Task 14's `capture_transcript`:
- the offset protocol (409 below the cursor, a counted gap above it);
- the project from the `X-Alice-Project` header, else the `project` query, as the in-process hint in tenant mode; from `cwd` locally (P8);
- the `no_capture`/`no_distill` flags.

**Tests** (service-level, no HTTP):
- **from Task 6:**
  - `a_continuation_file_skips_copied_turns`;
  - `subagent_files_are_their_own_sources`, through an upload with `subagent`;
  - `an_excluded_project_moves_the_cursor_and_records_keys`;
  - `a_fail_closed_project_waits_and_keeps_the_cursor`;
  - `block_mode_withholds_a_chunk_and_keeps_capturing`;
  - `twenty_sessions_at_once_capture_everything_once_beside_remembers` (uploads);
  - `a_forty_megabyte_transcript_is_captured_in_bounded_passes` (uploads of at most 8 MiB);
- **from Task 14:**
  - `an_upload_captures_complete_turns_and_returns_the_cursor`;
  - `a_lower_offset_gets_409_with_the_cursor`;
  - `a_resend_from_zero_stores_nothing_twice`;
  - `a_forward_gap_is_accepted_and_counted`;
  - `the_server_redacts_and_never_reads_paths`;
  - `the_project_hint_shares_the_mcp_placeholder` (header and query);
  - `no_capture_and_no_distill_flags_are_honoured`;
- **new:** `a_late_tail_is_stored_once_even_after_a_resend`.

### Task 14 (now): the HTTP routes and the emulator tests

**Routes:**
- `POST /api/v1/capture/transcript`, with its own `DefaultBodyLimit::max(TRANSCRIPT_BODY_LIMIT)`;
- `GET /api/v1/capture/status`;
- `GET /api/v1/capture/briefing`, together with `Service::capture_briefing` and `BriefingQuery`, moved here from Task 7.

**Tests** through `support::caddy::Server`:
- `the_body_limit_is_eight_mebibytes`;
- `an_adopted_database_is_not_captured_twice`;
- `the_tenant_briefing_resolves_the_hint`;
- `the_briefing_route_answers_204_when_off`;
- an end-to-end upload through the emulator that honours `X-Alice-Project`.

### Task 15: as written, plus the project hint

The project hint is computed as the owner's MCP `headersHelper` does: the basename of `git -C "$cwd" rev-parse --show-toplevel` when that top-level is not `$HOME`, with `"` and `\` removed; outside git, no hint (the project is `global`). It is sent as the header `X-Alice-Project` (through `curl -K -` lines, never argv) on the transcript and briefing calls. The token and URL come from `~/.config/alice/{token,url}`, per the amendment.

### Task 6b (before Task 7): local capture, reusing 6a's pipeline

**What it adds:**
- the notify endpoint and the worker thread;
- file reading, `file_key`, and streaming past an oversized line with a stub;
- settle, sweep and stale end (local);
- `SpoolEntry::CaptureNotify`.

**Tests:** the rest of Task 6's list: `a_stop_notify_captures_the_completed_turn`, `repeating_the_notify_stores_nothing_new`,
`a_kill_between_slices_neither_loses_nor_doubles`, `a_replaced_file_resets_and_skips_stored_turns`,
`the_trailing_turn_is_closed_by_the_quiet_sweep`, `a_stale_session_is_ended_by_the_sweep`,
`a_spooled_notify_is_drained_into_a_pass` and `the_tenant_daemon_refuses_notify_and_reads_no_files`.

### Other moves

- **Task 7** loses the briefing route and service method (now in Task 14). It keeps the hook client that calls them.
- **Task 12** also marks capture sources with no capture for 24 hours as ended (for tenants, which have no sweep) and makes
  them due.
