# Alice SP2 — Automatic Capture — Design Spec

- **Date:** 2026-10-16
- **Status:** approved by the controller with six rulings (`sp2-spec-review.md`, 2026-10-16) and by the owner, whose
  decision on the Jev key is folded in too. Nothing here is built. The changes:
  - **R13:** tenants get a bounded SessionStart briefing (§5.7);
  - **retention:** a capture-scoped retention, 180 days on tenants (§3.9, R25);
  - **adoption:** the owner becomes a tenant (§9.4, R26);
  - **samples:** permission for structure-only transcript samples, with conditions (§11.1, §13);
  - **the owner's decision:** Jev on for every tenant with one shared key the owner manages (§9.3, R27).
- **Owner:** icueth (vision); technical choices delegated to this lane and recorded as rulings (§12).
- **Lane:** SP2, worktree `project-alice-sp2`, branch `feat/sp2-capture`, from `origin/feat/server-mode` at `16641c8`
  (which contains M2 through Task 24 and M3).
- **Builds on:** the core spec `2026-10-13-alice-core-design.md` (§8.5 is SP2's contract), the server-mode spec
  `2026-10-15-alice-server-mode-design.md` (tenants, direct MCP, forced redaction, the team store), the M2 handoff and the
  M2 plan's Global Constraints, which bind this work too.
- **Addresses:** the repository is public. This spec writes `<host>` and `<name>` for the server and a tenant; no address,
  token or password appears in it or in anything SP2 commits.

---

## 0. Intent and scope

### 0.1 What the owner decided (vision; not re-asked)

1. **Start now**, in parallel with the final reviews of M2, M3 and server mode.
2. **Capture both.** Every session's transcript goes into the raw tier, where it is searchable. The session is also
   distilled into knowledge memories: a decision with its reason, a bug fix as symptom, cause and fix, a how-to, a warning,
   a preference, a fact, or a handoff.
3. **Jev distils.** Distillation sends redacted session text to the Jev judge, and only for projects whose cloud policy
   allows it. Other projects get the raw tier only. The owner already uses Jev on their own memories.

### 0.2 What the controller requires (binding; summarised from the lane brief)

- **Agents:** Claude Code first. Research Codex CLI and include it if it can capture a transcript; otherwise leave a seam.
- **Both deployments:**
  - **Local:** `alice hook <event>` (§8.5) finishes in under 100 ms, never blocks or fails the agent, and spools when the
    daemon is down.
  - **Server tenants:** the person installs nothing heavy (no compiler, no Alice binary). There is at most one
    copy-paste setup step. Capture goes to their own tenant over HTTPS with their bearer token, which is stored 0600 and
    never put in argv, URLs or logs. Redaction happens on the server.
- **Incremental and idempotent:** a re-run, a crash or a resumed session never duplicates chunks or memories.
- **No floods:** transcripts stay in the raw tier, distilled memories pass through the judge pipeline (so a memory the
  agent already `remember`ed is not stored twice), importance is sensible, tool outputs are trimmed, and binaries and
  images are never stored.
- **Cost and failure bounds:** distillation runs in the daemon's background, never in the hook. The per-day budget comes
  from the judge's settings, there is a breaker for Jev failures and a note in `alice doctor` and stats, and raw capture
  always works without Jev.
- **Settings** through the live settings layer and its loosening rules. **Install** with a command that backs up, merges,
  is idempotent and has documented removal. **Migration** from MemPalace's hooks and the current stopgap. **Quality** measured
  on a committed synthetic labelled set, plus a local check on a few of the owner's real sessions.

### 0.3 Success criteria (each one is checked in §11)

1. After `alice hooks install claude`, a Claude Code session's turns appear in the raw tier (`alice recall --raw`) within
   seconds of each turn ending, in the session's project.
2. `alice hook <event>` returns in under 100 ms at p95, with the daemon up and with it down. It always exits 0 and never
   blocks the agent.
3. Re-running a hook, killing the daemon mid-capture, truncating the cursor, or resuming or forking a session never
   stores a turn twice.
4. With `distill.provider = jev`, a session that has ended is distilled into at most `distill.max_per_run` memories per run.
   One session keeps at most `distill.max_per_session` memories over all its runs, its subagents included.
   One run keeps at most `distill.max_per_subagent_run` of them from subagent reports.
   They come from a project the cloud policy allows, carry provenance to their chunks, and are marked as distilled.
5. A memory the agent already `remember`ed in the session is not stored a second time by distillation.
6. Distillation never creates a review item, and transcripts never appear in the `context` briefing or as knowledge hits.
7. The day's Jev spend for distillation stays within its share of `judge.daily_budget_usd`. Three Jev failures in a row
   open the breaker, and `alice doctor` says so.
8. A server tenant's person runs one pasted command. From then on their Claude Code sessions are captured in their tenant
   over HTTPS, with no Alice binary. Their token is in a 0600 file only, and the server redacts the text. Their sessions
   start with the briefing, or with nothing within 2 s when the server is slow or down.
9. `alice hooks install claude` against a throwaway HOME backs up `settings.json`, merges without touching other hooks, is
   idempotent, and `alice hooks uninstall claude` removes exactly what it added.
10. On the committed synthetic set, distillation reaches the thresholds declared in §11.3. The owner's real sessions are
    checked locally before acceptance.
11. A MemPalace transcript already imported and the same session captured by SP2 do not both hold the same turns (§9).
12. Captured transcripts age out by `capture.raw_retention` (180 days on tenants), and imported history never does.
13. After the owner's database is adopted into their tenant, the tenant hook's resend from offset 0 stores no turn that was
    captured locally (§9.4).

### 0.4 Out of scope

- **Other agents** (Cursor, Windsurf, OpenCode, Gemini CLI). The transcript reader is a trait with one implementation per
  agent, so each one is a later reader plus an installer. Their `capture.*` flags already exist in the settings.
- **Other distillation providers** (`ollama`, `claude-cli`, `codex-cli`, OpenAI-compatible). `distill.provider` is an enum
  with `off` and `jev` now. A provider that names an endpoint must join tenant validation (server spec §2.2 item 6) when
  it arrives.
- **Abstractive summaries.** Jev answers choice questions only (§2.4), so a distilled memory is a passage of the session,
  chosen and classified by Jev, not new text (§6.1).
- **Distilling into the team store.** Distilled memories are personal. Only an explicit `remember shared:true` reaches the
  team store.
- **Windows** (not a target since server-mode addendum 2), the UI (SP3), and the `alice remote` client (deferred).

### 0.5 Anti-goals: a failure even if it technically works

- The briefing or the knowledge hits fill with transcript text or with near-copies of what agents already saved (the
  MemPalace failure).
- An agent waits on a hook, or a hook error shows up in the agent's session.
- The review inbox fills with questions about machine-made memories.
- A secret, or a project the owner kept away from the cloud, reaches Jev.
- Jev spend runs past the budget, or a Jev outage stops raw capture.

---

## 1. Overview

```
 LOCAL (the owner's Mac)                                   SERVER TENANT (a team member)
 ┌──────────────────────────────────────────────┐         ┌──────────────────────────────────────────────────┐
 │ Claude Code / Codex                           │         │ person's machine (no Alice binary)                │
 │   hooks: SessionStart Stop SubagentStop       │         │  Claude Code plugin "alice-capture" (local        │
 │          PreCompact SessionEnd                │         │  marketplace written by setup.sh)                 │
 │      │  stdin JSON {session_id, transcript_   │         │   hook → sh ~/.config/alice/alice-capture.sh      │
 │      ▼   path, cwd, hook_event_name}          │         │     forks to background, returns at once          │
 │ alice hook <event>   (< 100 ms, exit 0)       │         │     reads new bytes of the transcript file        │
 │   POST /api/v1/capture/notify ──┐             │         │     curl -K - (token from a 0600 file via stdin)  │
 │   (spool + detached start if    │             │         │       POST https://<host>/u/<name>/api/v1/        │
 │    the daemon is down)          ▼             │         │            capture/transcript?offset=…            │
 │ alice daemon                                  │         └───────────────────────────┬──────────────────────┘
 │   capture worker: reads the file from its     │                                     ▼ Caddy (unchanged)
 │   cursor ─┐                                   │         ┌──────────────────────────────────────────────────┐
 │           ▼                                   │         │ alice daemon --tenant                             │
 │  ┌─────────────── shared core ──────────────┐ │         │  ┌─────────────── shared core ──────────────┐     │
 │  │ parse JSONL → turns → render & trim →    │ │         │  │ (same code; the bytes come in the body,   │     │
 │  │ dedupe turns → redact → write chunks +   │ │         │  │  never from a path on the server)         │     │
 │  │ cursor in one transaction (raw tier)     │ │         │  └──────────────────────────────────────────┘     │
 │  └──────────────────────────────────────────┘ │         │  distill worker (the owner's shared Jev key)       │
 │  distill worker (background):                 │         └──────────────────────────────────────────────────┘
 │   session quiet/ended → candidates → one Jev  │
 │   choice request → memories (origin=distill,  │
 │   provenance) → normal judge pipeline         │
 └──────────────────────────────────────────────┘
```

The design has one **shared core** that turns transcript bytes into raw-tier chunks, and one **distiller** that turns new
chunks into knowledge memories. Only the transport differs between the deployments:

- **Locally**, the hook only tells the daemon "this session's file changed", and the daemon reads the file. The hook stays
  far below 100 ms whatever the file's size, and the cursor lives in the database.
- **On a tenant**, the server cannot read the person's files. A small shell script sends the new bytes, and the server
  keeps the cursor.

---

## 2. What the agents and Jev give us (research, 2026-10-16)

### 2.1 Claude Code hooks

Sources: `code.claude.com/docs/en/hooks`, `…/plugins-reference`, `…/plugins/install`.

- **Events SP2 uses:**
  - `SessionStart`, with `source` = `startup`, `resume`, `clear`, `compact` or `fork`;
  - `Stop` and `SubagentStop`, which carry `agent_id`, `agent_type` and `agent_transcript_path`
    (`…/<session>/subagents/agent-<id>.jsonl`);
  - `PreCompact`, with `trigger` = `manual` or `auto`;
  - `SessionEnd`, with `reason`.
  Every event carries `session_id`, `transcript_path`, `cwd` and `hook_event_name`.
- **Timing caveats:** the docs say `transcript_path` "may lag", and that at `Stop` the file is not guaranteed to hold the
  final message yet. `SessionEnd` hooks share a 1.5 s budget.
- **Output:** exit 0 is success. A SessionStart hook's stdout, or `hookSpecificOutput.additionalContext` (at most 10,000
  characters), is added to the context. Other exit codes are non-blocking, and the first stderr line is shown in the
  transcript.
- **Configuration:** `hooks.<Event>[] = {matcher?, hooks: [{type: "command", command, timeout?, async?}]}`, in
  `~/.claude/settings.json` (or under `$CLAUDE_CONFIG_DIR`), the project's settings files, or a plugin's
  `hooks/hooks.json`. Matching hooks run in parallel. Hooks wait for the workspace trust dialog in interactive sessions.
- **Plugins** can ship hooks. They install from a marketplace, which may be a **local directory**:
  `claude plugin marketplace add <path>`, then `claude plugin install <name>@<marketplace> --scope user` (inside Claude
  Code: `/plugin marketplace add`, `/plugin install`).
- **Transcripts (undocumented; community sources):**
  - one JSONL file per session at `<config>/projects/<encoded-cwd>/<session-id>.jsonl`;
  - line types `user`, `assistant`, `system` (subtype `compact_boundary`), `progress` and others;
  - fields `uuid`, `parentUuid`, `sessionId`, `timestamp`, `cwd`, `isSidechain`, `isMeta`;
  - a compaction appends a `compact_boundary` system line and then a `user` line with `isCompactSummary: true`;
  - a continued session can start a **new file with a new session id whose first records duplicate the parent's**;
  - assistant messages can be split across several lines that share `message.id`.
  Because the format is not a published interface, the reader is tolerant: an unknown line type is skipped and counted,
  never fatal (§3.3, §10).

### 2.2 Codex CLI

Sources: `learn.chatgpt.com/docs/hooks`, `…/config-file/config-advanced`.

- **Hooks exist and are on by default** (`[features] hooks = false` turns them off). They live in `~/.codex/hooks.json` or
  inline `[hooks]` tables of `config.toml` (one format per layer), plus project and plugin layers.
- **Events:** `SessionStart`, `Stop`, `SubagentStop`, `PreCompact`, `SessionEnd` and others. The stdin JSON carries
  `session_id`, `cwd`, `hook_event_name` and `transcript_path` (a string, or null). `SessionEnd` defaults to 1 s and allows
  at most 3 s.
- **Trust:** a non-managed hook runs only after the person reviews and trusts it in `/hooks`. Trust is tied to the hook's
  hash.
- **Transcripts:** "rollout" files `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (`$CODEX_HOME`). Each line is
  `{timestamp, type, payload}` with types `session_meta`, `turn_context`, `response_item`, `event_msg` and `compacted`. The
  docs warn that the format is not a stable interface. One source mentions `.jsonl.zst` files.
- The older `notify` program gets only `agent-turn-complete` with a `thread-id` and no transcript path. Hooks supersede it.

**Conclusion:** Codex can capture a transcript through its hooks. It is included as the second reader (§3.3, §4.3).

### 2.3 Why not Claude Code `http` hooks for tenants

Claude Code also has `type: "http"` hooks, which post the hook JSON to a URL. They cannot carry the transcript: the payload
holds `last_assistant_message` and prompts, but not tool context or compaction summaries. The token would also have to sit
in `settings.json`, which is not 0600, or in an environment variable. Rejected (R9).

### 2.4 Jev

Source: `docs.typesafe.ai/api.md`, `…/primitives.md`.

- **Question types:** `choice` (up to 255 options), `score`, `noul` (yes/no) and `box`. There is **no text-generating
  type**.
- **Limits:** any number of questions per request; a body of at most 32 MiB, within the model's context.
- **Price:** about $0.042 per million input tokens.
- **As built,** Alice's client asks `choice` questions only, with caching, budget reservation, a rate limit, redaction and
  the cloud gate.
- **Consequence:** SP2 distils **extractively**. Local code proposes candidate passages, and Jev classifies them in one
  `choice` request per run. SP2 adds no new question type (R17).

---

## 3. The capture core (shared by both deployments)

### 3.1 Sources and their identity

Each transcript file is one raw-tier `source` of type `session`.

| Agent | `sources.uri` | `agent` | `session_id` |
|---|---|---|---|
| Claude Code, main session | `claude-code:session:<session_id>` | `claude-code` | the session id |
| Claude Code, subagent | `claude-code:session:<session_id>:agent:<agent_id>` | `claude-code` | the parent session id |
| Codex | `codex:session:<session_id>` | `codex` | the session id |

- A session id or agent id must match `^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`; anything else is refused.
- The uri never holds a file path, so tenant mode stores no client path (server spec §2.8).
- `started_at` is the first captured record's time. `ended_at` is the last captured record's time, updated on each pass.
- **The source owns its project** (this settles M1's open ruling on re-ingest). The project is chosen when the source is
  first captured (§3.6), and every later chunk of the source takes the source's current project, so `alice projects merge`
  is followed.

### 3.2 Reading incrementally

Each capture source has a row in `capture_state` (§8.1). The row holds:

- the **cursor**: `byte_offset`, always at the start of a line, and `next_ord`;
- the local file's identity, `file_key` = device, inode and the SHA-256 of the first line (local only);
- distillation state (§6).

A pass works like this:

1. Read from `byte_offset`, at most 8 MiB per pass. Only complete lines (ending `\n`) are consumed.
2. A line over 4 MiB is skipped and leaves the stub `[record skipped: too large]` in its turn. Inline images are the usual
   cause.
3. If the file is shorter than the cursor, or its `file_key` changed, the file was replaced. The cursor goes back to 0, and
   turn dedupe (§3.5) keeps already-stored turns from coming back.

### 3.3 From records to chunks

The reader for each agent turns lines into **records**, groups records into **turns**, and renders each turn as text.

**Turns.**
- A turn starts at a real prompt: a `user` record whose content is text and not only tool results. Records marked
  `isMeta`, `isSidechain` (inside the main file) or holding only a command wrapper do not start one.
- A compaction summary (`isCompactSummary`, or Codex `compacted`) is a turn of its own.
- A turn runs until the next turn starts.

**When a turn is complete** (only complete turns are captured):
- a later turn exists; or
- the pass was triggered by `Stop`, `SubagentStop`, `PreCompact` or `SessionEnd` and has waited the settle delay (1.5 s
  locally; the tenant script waits 2 s); or
- the file has been unchanged for 5 minutes (sweep).

The cursor stops at the start of the first incomplete turn, so that turn is re-read on the next pass. Capture therefore
lags the live session by at most one turn.

**Late records.** A record may arrive after its turn was stored, despite the settle delay. Records found at a cursor
greater than 0, before any turn starts, are the late tail of the source's previous turn. They are rendered as
continuation chunks of that turn: the same source, the next ords, no new turn key, in the same transaction as the cursor.
Records found at offset 0 before any turn starts (the head of a continuation file) form a turn of their own, keyed by
their first record (§3.5).

**Rendering contract.** The distiller (§6.3) parses chunks back by these tags, so the format is a contract. Each block is
a line that starts with its tag:

```
[user] <prompt text>
[assistant] <text>
[tool Bash] git status --short
[result] <trimmed output>
[result error] <trimmed output>
[summary] <compaction summary>
[subagent general-purpose] <the subagent's final report>
```

**What is kept, trimmed or dropped.** Trimming is head plus tail, cut at UTF-8 boundaries, with a marker
`[… 12,345 characters trimmed …]`. *N* is `capture.tool_output_chars`, default 800: the first 500 and the last 300.

| Content | Rendering |
|---|---|
| User prompt text | Kept. `<system-reminder>…</system-reminder>` spans are removed (they are injected context such as CLAUDE.md and Alice's own briefing). Command wrappers keep the command name and at most 300 characters of output |
| Assistant text | Kept |
| `thinking` blocks | Dropped (`capture.include_thinking = false`) |
| Images, documents, audio | `[image]`, `[document]`, `[audio]`. The bytes are never stored |
| Read / NotebookRead | `[tool Read] <path>`, no content: Alice does not index code (core spec §0.4) |
| Edit / Write / MultiEdit / NotebookEdit | `[tool Edit] <path>`, no content |
| Bash | `[tool Bash] <command, ≤ 300 chars>`, plus the result trimmed to *N* |
| Grep / Glob / LS | the pattern, plus the result's first 300 characters |
| WebFetch / WebSearch | the URL or query, plus the result's first 500 characters |
| Task / Agent (subagent call) | `[subagent <type>] <description>`, plus the subagent's final report up to 2,000 characters |
| TodoWrite and similar bookkeeping tools | Dropped |
| Alice's and MemPalace's own tools (`recall`, `context`, `mempalace_search`, `remember`, `mempalace_add_drawer`, …) | `[tool alice remember] <title or first line, ≤ 120 chars>`. Results become `[result omitted: memory tool]`. Memory text never re-enters the raw tier, and the distiller reads these lines as "already saved" (§6.3) |
| Other tools | `[tool <name>] <input as compact JSON, ≤ 300 chars>`, plus the result trimmed to *N* |
| Errors (`is_error`) | `[result error]`, trimmed to *N* |
| Runs of 200 or more base64-like characters, or text over 30 % unprintable | `[binary removed]` |
| `progress`, `file-history-snapshot`, title `summary`, `system` lines, hook attachments | Dropped |

**Streaming duplicates.** Within a turn, a content block identical to one already rendered for the same `message.id` is
skipped.

**Chunks.**
- A rendered turn is cut into chunks of at most `capture.chunk_chars` characters (default 2,000, and never over the
  32,768-byte chunk limit).
- Cuts fall at block boundaries. An oversized block is cut at paragraph ends, then sentence ends, then hard; its
  continuation chunks start with `<tag> (cont.)`.
- Cutting depends only on the turn's records, so a re-render is byte-identical.
- A chunk's `authored_at` is its first record's timestamp.

**Codex.** The rollout reader applies the same turn rule (a user message that is not tool output starts a turn) and the
same rendering table. Its line types are mapped in the plan, from fixtures built after the format above. A file ending
`.zst` is skipped in v1 and counted (R21).

### 3.4 Subagents

- A Claude Code subagent file (`SubagentStop`'s `agent_transcript_path`) is its own source, linked to the parent through
  `capture_state.parent_source_id`. It is captured when `capture.include_subagents` is on (default).
- Subagent sources whose whole rendering is under 200 characters are dropped (internal helpers such as prompt suggestions
  also fire `SubagentStop`).
- Distillation reads only a subagent's final report (§6.3).

### 3.5 Dedupe: exactly once per turn

Every captured turn records its **turn key** in `capture_turns(agent, turn_key)` (§8.1), in the same transaction as its
chunks.

- **The key:** the `uuid` of the turn's first record. A record without a uuid (Codex) uses SHA-256 of its timestamp and
  its first 200 characters.
- **The rule:** a turn whose key is already stored, in any source, is skipped.

This one rule covers everything that could duplicate:

| Case | What happens |
|---|---|
| A hook fires twice | The second pass finds the cursor past the turn and reads nothing |
| The daemon is killed mid-pass | The cursor and the turn key commit with the turn's last chunk slice. A turn whose earlier slices landed is re-rendered identically, and `(source, ord)` uniqueness drops the repeats |
| A continued or forked session starts a new file that copies the parent's records | The copied turns carry the parent's uuids, so they are skipped. The new turns are stored |
| A file is replaced or truncated | The cursor resets and the stored turns are skipped |
| A tenant client loses its state | It is told the server's cursor (§5.2). Even if it resends from 0, the turns are skipped |
| Raw retention deletes an old source | Its turn keys stay, so the still-existing file is not captured again |

### 3.6 The project of a source

- **Locally:** the hook's `cwd` goes through the daemon's existing detection (git root, then normalised remote, then
  slug), as for a `remember` with a `cwd`.
- **On a tenant:** the script sends the basename of the git top-level, or of `cwd`, as a project hint. The server
  slugifies it and resolves it exactly as the direct-MCP path does (server spec §2.8 step 3): an existing slug or alias,
  else the `import:<slug>` placeholder. The person's MCP writes and captured transcripts of one repository therefore land
  in one project.
- **Exclusions are checked on every pass:**
  - `capture.enabled`;
  - the agent's flag (`capture.claude_code`, `capture.codex`);
  - `capture.excluded_projects`;
  - the project's `.alice.toml` `[privacy] no_capture = true` (a new tightening-only key, §7);
  - on a tenant, the client's `no_capture` flag.
  An excluded project's new turns are **not stored and the cursor moves past them**, so un-excluding a project later does
  not backfill the excluded period. Their **turn keys are recorded** (with no chunks), so a continuation file that copies
  those records does not capture them either.
- A continuation file (a new session id) gets its project from its own `cwd`, like any new source. A continued session
  runs in the same working tree, so it lands in the same project.

### 3.7 Redaction

- Every rendered chunk passes the redactor of its source's project (`Redactors::for_root`: the global `privacy.*` plus
  the project's `.alice.toml`; on a tenant, redaction is forced on, server spec R28).
- In `block` mode a chunk with a secret is replaced by the stub `[withheld: <detector names>]`, and its turn still counts
  as captured. One secret must not stop a session's capture, and nothing with a secret is stored.
- Paths in `capture_state.transcript_path` are not redacted. They exist only locally, are needed to read the file, and are
  never shown to agents.

### 3.8 Writing

A new writer operation, `WriteOp::CaptureSlice`, takes:

- the source (created if new);
- the source's project;
- up to 50 chunks or 32 KiB of text (the ingest slice rule);
- the turn keys that end within the slice;
- the cursor after the last complete turn in the slice.

It inserts the chunks (`INSERT OR IGNORE` on `(source_id, ord)`), the turn keys and the cursor in **one transaction**. The
capture worker pauses 2 ms after each commit, as the importer does, so `remember` waits for one slice at most. Chunks are
embedded by the existing worker; with a remote embedder, the cloud policy holds back a denied project's chunks as today.
A pass that stored something records one `source.capture` history event (like `source.ingest`, not undoable).

### 3.9 Volume, retention, and what it costs

**Rate.** With the trimming above, a turn renders to about 1–3 KB, about one chunk on average. The owner runs about 20
sessions a day of about 50 turns each, so **~1,000 chunks a day, ~365,000 a year**. That is within the 1M-chunk search
budget, on top of the 121k imported.

**Disk on the server.** The controller measured about **16.8 KB per chunk** on the server, counting the database with its
text, FTS rows and vectors, plus two scheduled backups. The rates below are estimates; the real-session check measures the
owner's actual rate and reports it.

| Who | Chunks/day | Kept at 180 days | Disk at 180 days |
|---|---|---|---|
| The owner | ~1,000 | ~180,000 | ~3.0 GB |
| Each of the other three tenants (assumed 300/day) | ~300 | ~54,000 | ~0.9 GB each, ~2.7 GB together |
| **All four** | ~1,900 | ~342,000 | **~5.7 GB** |
| All four at the owner's pace (worst case) | ~4,000 | ~720,000 | ~12.1 GB |

**Why the cap is needed.** Without a cap, the owner alone adds ~6.1 GB a year and keeps growing. The disk is not grown,
19 GB were free, and §2.7 of the server spec already plans ~8 GB for homes and backups. A year of uncapped capture would
fill it.

**What it does:**
- **`capture.raw_retention`** (§7) ages out raw chunks of **capture sources only**: sources with a `capture_state` row,
  i.e. uris `claude-code:…` and `codex:…`.
- **Imported MemPalace history and other raw sources are never touched by it**; they follow `storage.raw_retention` alone,
  as today.
- **Defaults:** `forever` locally; **180 days on tenants**, set by `alice-server` (§9.5).
- **The worst case** at 180 days does not fit beside the planned homes. The admin's lever there is `90d` for the heaviest
  tenant, and the guard order of server spec §2.7 still applies.

**How it runs.** The existing `raw_retention` maintenance job gains a second pass with the same rules:
- by age, counted from when Alice stored the chunk (`created_at`), at least `1d`; or by size of capture-chunk text, at
  least `1MB`;
- deletes in slices of at most 500 rows;
- one `raw.retention` event per run.
Deleting a source's old chunks leaves its cursor and its turn keys, so the live file is not captured again (§3.5).
Distilled and explicit memories stay; their provenance rows to deleted chunks go with the chunks, as today.

**Embedding.** A tenant's ~300–1,000 chunks a day take 1–3 minutes of CPU at the server's 5.5 chunks/s.

---

## 4. Local deployment

### 4.1 `alice hook <event>`

`alice hook <event> [--agent claude-code|codex]`. The events are `session-start`, `stop`, `subagent-stop`, `pre-compact`
and `session-end`; the stdin `hook_event_name`, when present, wins over the argument.

1. **Read the input.** Read stdin to its end (at most 1 MiB) and parse `session_id`, `transcript_path`, `cwd`,
   `agent_id`, `agent_transcript_path`, `source` and `reason`. Input without a session id or path is ignored.
2. **Notify the daemon.** Discover the daemon as clients do today (`daemon.json`, then the ping proof), with hook-specific
   timeouts: 30 ms for the ping and 40 ms for the request. Then send `POST /api/v1/capture/notify`.
3. **Print the briefing on SessionStart.** Only for `source` = `startup`, `clear` or `compact`: on `resume` and `fork` the
   restored conversation already holds one. `capture.briefing = false` turns this off. The hook calls
   `GET /api/v1/capture/briefing` for the session's `cwd` within the remaining budget (the same route the tenant script
   uses, §5.7), and prints
   `{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"<briefing>"}}`. The briefing starts
   `# Context · <project>`, which the MCP instructions already treat as "a briefing is in the conversation". This replaces
   the stopgap script's `alice context`.
4. **When the daemon is unreachable,** write a `capture_notify` spool entry (one fsynced file, as today) and start the
   daemon detached, unless `ALICE_NO_AUTOSTART` is set. On SessionStart nothing is printed; the MCP instructions make the
   agent call `context` itself.
5. **Always exit 0.** Problems go to `logs/hook.log` (rotated at 1 MB; no transcript text), never to stderr.

**Budget:** under 100 ms from process start to exit at p95, measured in a release build with the daemon up and with it
down, for **every** event, `session-start` with its briefing included (§11).
- **The timeouts are caps, not expected costs.** Locally a ping, a notify and a `context` call each take a few
  milliseconds: `context` measured 0.5 ms p95 in the daemon (core spec §4.6), and the `alice` binary starts in about 5 ms
  (M2's `alice mcp` measurement).
- **The whole hook has a hard deadline of 90 ms.** A briefing that has not arrived by then is not printed; the agent then
  calls `context` itself, as the MCP instructions say.

The installer writes plain synchronous command hooks with `timeout: 10` (seconds) as a guard; it does not use `async`
(R7).

### 4.2 The capture worker

The capture worker is a daemon thread, `alice-capture`, at low priority.

- **Notify** (`POST /api/v1/capture/notify`) validates the request and answers 202 at once. It is refused in tenant mode:
  "this server cannot read files on your machine; use the capture script (docs/capture.md)".
- **Queue.** Notifications are queued and coalesced per source.
- **Register.** The first notification of a session registers its source and stores `transcript_path` and its project.
  Later ones are passes.
- **Path check.** The file must be a regular file whose name ends `.jsonl`, opened read-only. There is no root allowlist
  (R10).
- **Settle.** A pass caused by `Stop`, `SubagentStop`, `PreCompact` or `SessionEnd` waits the settle delay (1.5 s) first.
- **Sweep.** Every 60 s, and at daemon start, each source that has not ended is read again if its file grew or its
  trailing turn has been quiet for 5 minutes. A source with no event for 24 hours is marked ended after a final pass
  (an agent that crashed sends no `SessionEnd`).
- **Spool.** A drained `capture_notify` spool entry is an ordinary notification. Because the daemon reads from its cursor,
  a lost or repeated notification costs nothing.
- **Codex paths.** A Codex `transcript_path` of null is found by searching `$CODEX_HOME/sessions` for a rollout file that
  names the session id, over the newest 7 date folders. Nothing found is counted for `alice doctor`.

### 4.3 `alice hooks install | uninstall | status <claude|codex>`

**Claude Code.**
- **Target:** `$CLAUDE_CONFIG_DIR/settings.json`, else `~/.claude/settings.json`; `--settings <path>` overrides it.
  `--dry-run` prints the change and writes nothing.
- **Missing or unparseable file:** a missing file starts from `{}`. An unparseable file is refused with nothing changed.
- **Backup:** before any write the file is copied to `settings.json.alice-backup-<UTC stamp>` (0600). The newest 5 such
  backups are kept.
- **What it writes:** for SessionStart (no matcher), Stop, SubagentStop, PreCompact and SessionEnd, one entry
  `{"hooks":[{"type":"command","command":"'<abs alice>' hook <event> --agent claude-code","timeout":10}]}`.
  - `<abs alice>` is the canonical path of the running binary.
  - The command starts with `ALICE_HOME='<home>' ` when the install ran with a non-default `ALICE_HOME`, so tests and
    second homes capture into the right place.
- **Which entries are Alice's:** a command whose program's file name is `alice` and whose arguments are
  `hook <event> --agent claude-code`, whatever its path. Install removes all of Alice's entries, then adds fresh ones. It
  is therefore idempotent and repairs hooks that point at a moved binary.
- **Other content** (other hooks, other keys) is left as it is. The write is atomic (temporary file, then rename). Key
  order and formatting are kept as far as practical; the plan picks the means without changing JSON output elsewhere
  (R20).
- **Output:** what changed, the backup path, the uninstall command, and that hooks load in new sessions. It also lists
  other hooks whose command mentions `mempalace` or `alice` (for example the stopgap `alice-session-start`), so the person
  sees what to remove (§9).
- **`uninstall claude`** removes exactly Alice's entries (and arrays left empty), with a backup.
- **`status`** shows whether the hooks are installed, whether their binary exists, and the last capture.

**Codex.**
- **Target:** `$CODEX_HOME/hooks.json`, else `~/.codex/hooks.json`, with the same rules.
- **Refusal:** if `config.toml` already holds inline `[hooks]` tables, install refuses with instructions, because Codex
  wants one format per layer.
- **Output** ends with "open `/hooks` in Codex once and trust the Alice entries".
- Codex `SessionStart` output is not used; the MCP instructions cover the briefing.

**Safety.** Tests run only against a throwaway HOME (`HOME`, `CLAUDE_CONFIG_DIR` and `CODEX_HOME` pointed into a
temporary directory). This lane never runs the installer against the real `~/.claude` or `~/.codex`.

### 4.4 Backfill

`alice capture backfill <claude|codex> [--since YYYY-MM-DD] [--project <slug>] [--distill]` notifies each existing
transcript file as a sweep (complete turns only).

- **Claude Code files:** `<config>/projects/*/*.jsonl` and their `subagents/`. **Codex files:** `sessions/**/rollout-*.jsonl`.
- Turn dedupe and the MemPalace rule (§9.2) prevent duplicates.
- Backfilled sessions are **not** distilled unless `--distill` is given (`distill_state = skipped_backfill`), because a first
  backfill of months of sessions must not become a burst of memories or spend.

---

## 5. Server tenants

### 5.1 Mechanism, and why

The person needs **`sh`, `curl` and Claude Code**, nothing else.

- **The setup script.** One pasted command fetches a setup script from their own tenant. The script:
  - reuses the token and URL files the team's setup document already installed (`~/.config/alice/token` and
    `~/.config/alice/url`), and asks for the token only when the file is missing or the token is refused;
  - installs a POSIX shell hook script;
  - writes a tiny Claude Code plugin into a **local marketplace** directory and installs it with `claude plugin`. Claude
    Code then merges the hooks itself.
- **The hook script** forks into the background at once. It sends the transcript's new bytes to
  `POST /api/v1/capture/transcript` with curl, reading the token from the file and passing it to curl on stdin.

Alternatives considered:

| Option | Why not |
|---|---|
| The Alice binary on the person's machine | The controller's requirement: no Alice binary |
| `http` hooks posting the hook JSON | They carry no transcript (§2.3), and the token would sit in settings |
| Editing `~/.claude/settings.json` from `sh` | That needs a JSON tool. `jq` is not on stock macOS, and `python3` opens an install dialog without the Command Line Tools. A plugin lets Claude Code merge and remove the hooks cleanly |
| A plugin from the public GitHub repository | Version skew between the plugin and the tenant's API, and a dependency on a branch. The local marketplace comes from the person's own tenant, at its version |

### 5.2 `POST /api/v1/capture/transcript`

- **Auth:** the existing bearer guard. Caddy already routes `/u/<name>/api/v1/*`, so no Caddy change is needed.
- **Query:**
  - `agent` (`claude-code` | `codex`);
  - `session`, and optional `subagent`;
  - `offset` (the byte offset of the body's first byte in the file);
  - `event` (`session-start` | `stop` | `subagent-stop` | `pre-compact` | `session-end` | `sweep`);
  - `project` (the raw basename, slugified on the server);
  - optional `no_distill=1` and `no_capture=1`.
  None of these is a secret. A project name in Caddy's access log is accepted by server spec R26.
- **Body:** raw transcript bytes, `Content-Type: application/x-ndjson`, at most 8 MiB (the route's own limit).
- **Cursor check, against the server's `capture_state.byte_offset` for the source:**
  - An `offset` equal to it, or a first offset of 0, is processed.
  - An `offset` lower than it answers `409 {"next_offset": <cursor>}` without parsing: the client lost its state, jumps
    and sends again.
  - A higher `offset` is accepted as a gap (the server's copy is older, e.g. after a restore). The gap is noted in
    `alice capture status`.
- **Processing:** only complete lines in the body are consumed. A body of at least 8 MiB with no newline answers
  `{"oversized_line": true}`, and the client skips that line (§5.3).
- **Answer:** `{"next_offset", "turns_stored", "turns_skipped", "chunks_stored", "ended"}`. `next_offset` stops at the
  start of the first incomplete turn, unless `event=session-end`, which closes the trailing turn. `ended` is `true` when
  this upload ended the session's source: a `session-end` whose pass wrote (its last body, or an empty body at the
  cursor). It is `false` when the pass stopped before writing (the rules wait, say), so the script never reports an end
  that did not land (Task 15b).
- **Pipeline:** the same core as locally (§3), with the bytes taken from the body and never from a path.
- **Project:** the route sets the in-process `project_hint` from `project` before `Service::choose` runs. The wire
  `project_hint` stays cleared on every other route, as today. The capture route is a binary-less entry point like direct
  MCP, which is the one other place a hint is set.
- **Local daemons accept it too**, which the tests use.
- **No compression** in v1. It would need a new dependency for request decompression, and the hook runs in the
  background, so upload size does not reach the agent (R22).

### 5.3 The hook script `alice-capture.sh`

The hook script is POSIX `sh`, served by the daemon at `GET /api/v1/capture/hook.sh` and kept by setup as
`~/.config/alice/alice-capture.sh` (0700). It needs `sh`, `curl`, `sed`, `head`, `tail`, `mkdir`, `mv` and `date`; `git`,
`awk` and `dash` are optional. The plugin's hook command is `sh "$HOME/.config/alice/alice-capture.sh" hook claude-code
|| true` (a missing script never fails a hook).

Its first three lines are `#!/bin/sh`, a marker line `# alice-capture-hook: …` and a version line
`# alice-capture-version: <v>`. The committed file says `unstamped`; the route serves it with `<v>` = the first 12 hex
digits of the SHA-256 of the compiled-in text, so the version changes exactly when the script does. The script also
prints the hooks files (`alice-capture.sh print-hooks-json claude-code|codex`): setup writes what it prints, and the
self-update (step 8) compares with it, so the hooks are written down in one place.

1. **Read the input.** Read stdin (at most 1 MiB). Extract `hook_event_name`, `session_id`, `transcript_path`, `cwd`,
   `agent_id` and `agent_transcript_path` with a JSON-string `sed` pattern that undoes `\\`, `\"` and `\/`. A value holding
   a `\u` escape is not guessed: that event is skipped and logged.
2. **Send the work to the background.** Pass the extracted fields to a background copy of the script, as arguments (no
   token among them), started with `nohup sh … >/dev/null 2>&1 </dev/null &`. Then `exit 0`, except on `SessionStart`,
   which first prints the briefing (step 5, at most 2 s). The agent never waits on capture.
3. **In the background:**
   - Take a per-file lock, a `mkdir` lock holding the holder's `pid` and `at` (refreshed every pass). Both are written
     through a temporary file and `mv`, so a reader never sees them empty, and each write moves the directory's mtime.
     - A live holder is broken only when its `at` is 900 s old, a dead one at 360 s.
     - Without a usable `at` (a holder killed between `mkdir` and writing it, or inside its unlock, or a full disk), the
       directory's own age stands in: more than 15 minutes for a live pid, more than 6 otherwise.
     - A dead or unknown holder without `at` is also broken after more than 5 tries (a hook waits up to 40 s).
   - Wait 2 s on stop-type events (the transcript may lag).
   - Read the stored offset from `~/.config/alice/state/<agent>-<session>[-<agent_id>].offset`.
   - Loop at most 16 times:
     1. `tail -c +<offset+1> <file> | head -c 8388608 > <tmp>` (a temporary file in a 0700 directory);
     2. post it;
     3. take `next_offset` from the answer with `sed` and store it atomically (write, then `mv`);
     4. stop when nothing is left or nothing was consumed.
   - On `409`, store the given offset and continue. On `oversized_line`, measure the line with `LC_ALL=C awk` and skip it.
   - On a network error or a 5xx, keep the offset; the next hook retries, because the transcript file itself is the queue.
   - On `401`, log "the token was refused; run the setup command again" and pause for 1 hour (marker file).
4. **Sweep.** On `SessionStart`, also run up to 5 other state files whose transcript has grown, so a session that ended
   while the server was unreachable is finished later.
5. **Briefing on `SessionStart`.** Before exiting, the foreground part prints the briefing as §5.7 says. It is the one
   synchronous network call, bounded to 2 s, and the capture work has already been sent to the background.
6. **Project and privacy.**
   - The project hint is the basename of `git -C <cwd> rev-parse --show-toplevel`, else of `cwd`.
   - If that top-level (or `cwd`) has a `.alice.toml` with a line `no_capture = true`, the event is skipped entirely. A
     line `cloud_denied = true` sends `no_distill=1`.
   - If the file exists but cannot be read, the event is skipped (fail closed).
7. **Log.** `~/.config/alice/capture.log`, kept under 256 KB, never holds the token or transcript text.
8. **Self-update (Task 15b).** The script updates itself, so a fix reaches every machine without anyone re-running
   setup.
   - **When.** At the start of a background worker's run, before its capture work, so a release whose capture path
     fails can still fetch its fix. Never in the hook itself, and at most once a day.
     - `~/.config/alice/update-checked` records when the last check started. Setup writes it too, so its install counts
       as that day's check.
     - A record in the future, or a garbled one (a leading 0, more than 11 digits), counts as due.
     - The check runs under a `mkdir` lock, `state/update.lock`, with the stale rules of step 3 (an `at`, else the
       directory's age, so a lock left by a killed run never wedges the check), and reads the record again under it.
       While another worker holds the lock, nothing is checked.
     - It writes the record before fetching and reads it back. When the record cannot be written, nothing is fetched,
       or every event would fetch.
     - Under the lock it also removes temporary files a killed run left, once they are more than 10 minutes old.
   - **The fetch.** `curl -q -fsS --connect-timeout 5 --max-time 30 --max-filesize 1048576` of
     `<url>/api/v1/capture/hook.sh`, the person's own tenant. The route is public, so no token is sent.
     - **`https://` only** (controller's ruling). With a loopback `http://` URL, capture works but the check is skipped,
       with one log line a day. The test-only switch `ALICE_CAPTURE_TEST_UPDATE_HTTP=1` lets the tests use the
       emulator.
     - Any answer other than 200 is refused, and every refusal line names the HTTP status.
   - **Deciding.** It replaces the script only when the served version line differs from the installed one.
   - **Checks before replacing.** The download must:
     - be non-empty, start with `#!/bin/sh`, and carry the marker and version lines;
     - end with `main "$@"`, so a cut-short download is refused;
     - pass `sh -n`, and `dash -n` where `dash` exists;
     - pass a smoke run: `sh <download> print-hooks-json claude-code`, with the shell the hook uses, must exit 0 within
       20 s and print JSON. A script that dies as it starts would stop capture and its own updates for good.

     The 20 s (and the 120 s of the `claude` calls below) are enforced by a watchdog, since macOS has no `timeout`: TERM
     at the limit, then KILL 2 s later when TERM did not end the command (a script's own TERM trap waits for its
     foreground command). The watchdog sleeps one second at a time. The caller stops it with KILL and kills its sleep,
     so none outlives the call, and it signals only while a sentinel file the caller removes exists, so never a reused
     pid. A child the command started can outlive the KILL.
   - **Replacing.** The download is a temporary file beside the script, `chmod 700`, then `mv`. A running worker keeps
     reading the old file. The log says `capture hook updated <old> → <new>`. Any failure (curl, a check, the `mv`) keeps
     the installed script byte for byte and logs one line. No log line holds the token or the script's text. A script
     removed meanwhile (uninstall) is never put back.
   - **The hooks files.** After the check, the plugin's `hooks.json` is compared with what the installed script prints,
     and rewritten only when it differs. Then:
     1. the plugin's version moves to `<its version>+<script version>` in `plugin.json` and `marketplace.json`, before
        `hooks.json` is rewritten (atomically);
     2. `claude plugin marketplace update alice-local` and `claude plugin update alice-capture@alice-local` run in the
        background: from `$HOME`, with stdin from `/dev/null` and `DISABLE_AUTOUPDATER=1`, each killed after 120 s
        (a watchdog subshell, since macOS has no `timeout`);
     3. when `claude` is missing or those commands fail, one log line says to re-run the setup command.

     Claude Code 2.1 reads a local-directory plugin from its own folder at each session start (checked under a throwaway
     `HOME`). A Claude Code that runs plugins from its cache copies only a new version, which is why the version moves.
     The Codex `hooks.json` follows only while it is still byte for byte the file setup wrote (`codex-hooks.written`). It
     is rewritten with its record, and the log reminds the person to trust the hooks again in `/hooks`.
   - **Across versions.** After an update, a hook of the old version starts the new file's `run`. So the `run`
     arguments, the state file's format, the lock layout, the `update-checked` record and the `print-hooks-json` output
     stay compatible for at least one version. The script's header says so.
   - **Repair.** Re-running the setup command is still how to repair an install, for example one whose token was
     refused or whose plugin files were removed. Local installs need none of this: the `alice` binary is the hook.
   - **Future work.** The script is not signed yet. The server now delivers code to every machine daily, so signing is
     planned: a key held off the server, and `ssh-keygen -Y verify` on the download before it may replace the script.

### 5.4 The setup command (the one copy-paste step)

The admin gives each person one line:

```
curl -fsSL https://<host>/u/<name>/api/v1/capture/setup.sh | sh -s -- https://<host>/u/<name>
```

`GET /api/v1/capture/setup.sh` and `GET /api/v1/capture/hook.sh` are **public**, like `/ping`: there is no token on that
path, but the Host and Origin guard still applies. Both are static text compiled into the binary. They hold no secret and
no address, and they are in the public repository anyway. The setup script:

1. **Finds the URL.** It uses `~/.config/alice/url` when the file exists (the team's setup document writes it, 0600). If
   the command's argument differs from it, the script says so and keeps the file. Otherwise it takes the argument and
   writes the file (0600, in a 0700 `~/.config/alice`). The URL must be `https://`, or `http://` to `127.0.0.1` or
   `localhost` (tests only).
2. **Finds the token.** It uses `~/.config/alice/token` (0600, the bare token on one line) when the file exists. It asks
   only when the file is missing, or when step 3 refuses the token. It asks from `/dev/tty` with echo off, and writes the
   answer to that same file (0600). It never takes the token from argv or the environment, and never keeps a second copy.
3. **Verifies the token** with `GET <url>/api/v1/capture/status`. The `Authorization` header goes to curl through
   `-K -` from `printf` (a shell builtin), so the token is never in a process's argv. A refused token from an existing
   file is reported, and the script asks once for a new one.
4. **Writes `~/.config/alice/capture.conf`** (0600), which holds only non-secret options: `briefing=on`. The person may
   change it to `off`.
5. **Installs** `alice-capture.sh` from `<url>/api/v1/capture/hook.sh`, after the checks of §5.3 step 8 (shebang,
   marker and version lines, `main "$@"` last, `sh -n` and `dash -n`). It writes `update-checked`: this install is the
   day's update check.
6. **Writes the local marketplace** `~/.config/alice/claude-marketplace/`, containing
   `.claude-plugin/marketplace.json` (marketplace `alice-local`) and the plugin `alice-capture/`. The plugin holds
   `.claude-plugin/plugin.json`, at the tenant's version, and `hooks/hooks.json`, with the five events calling the script.
   `hooks.json` is what `alice-capture.sh print-hooks-json claude-code` prints. The Codex file of step 8 is what
   `print-hooks-json codex` prints.
7. **Installs the plugin:** `claude plugin marketplace add ~/.config/alice/claude-marketplace`, then
   `claude plugin install alice-capture@alice-local --scope user`. If the `claude` command is missing or too old, it prints
   the two `/plugin` commands to type inside Claude Code instead.
8. **Codex:** if `codex` is installed and `$CODEX_HOME/hooks.json` does not exist, it writes one with the same five events.
   If the file exists, it prints the entries to add. Either way it reminds the person to trust them in `/hooks`.
9. **Prints** what it did, how to check (`sh ~/.config/alice/alice-capture.sh status`) and how to remove it.

The hook script then updates itself once a day, and the plugin's `hooks.json` with it (§5.3 step 8). Re-running the
command is how to repair an install, and it updates everything in place. `sh ~/.config/alice/alice-capture.sh uninstall`:
- uninstalls the plugin and removes the marketplace;
- removes the Codex `hooks.json` only if it is still exactly the one setup wrote;
- deletes the capture files after asking: the script, `state/`, `capture.conf`, `capture.log`, `paused-until`,
  `update-checked` and the marketplace. It never deletes `token` or `url`, which belong to the person's MCP setup.

Whether `alice-server` prints each person's line is the server lane's call. `docs/server.md` documents the line either way.

### 5.5 The token

- It is kept in `~/.config/alice/token` (0600, the bare token on one line) only. The same file serves the person's MCP
  setup, so a rotation is one edit (controller's interface, 2026-10-16). Scripts read it with `head -n 1` into an
  unexported shell variable, and the URL from `~/.config/alice/url` the same way.
- It reaches curl only through `curl -K -` with `header = "Authorization: Bearer …"` written by `printf`. It is never in
  argv, a URL or the environment of a child process, and never logged.
- **No ping proof** is checked by the shell. Verifying the HMAC with `openssl` would put the token in argv. TLS with the
  tenant's public certificate pins the host instead.
- **Cost:** a person who configures `/u/bob` with Ann's token sends Ann's token, inside TLS, to Bob's daemon. That daemon
  refuses it and never logs it, and the admin can read every tenant anyway (server spec Q1) (R12).

### 5.6 Privacy on this path

It matches the direct-MCP path (server spec §2.8):

- The text arrives unredacted inside TLS. The tenant redacts it with forced redaction and its own rules before anything
  is stored or logged.
- A repository's `.alice.toml` is not mirrored. The script honours its `no_capture` and `cloud_denied` lines as client-side
  flags (§5.3 step 5), and the tenant's `capture.excluded_projects` and `privacy.cloud_denied_projects` decide the rest.
- Full `.alice.toml` rules (custom patterns, block mode) on a tenant wait for the deferred `alice remote` client. This is
  said plainly in `docs/server.md`.

### 5.7 The SessionStart briefing on tenants (bounded)

The team uses only the server. Without this step they would never get the briefing that local users get (controller's
ruling, R13).

- **When:** on `SessionStart` with `source` = `startup`, `clear` or `compact`, as locally.
- **The call:** the script asks
  `GET <url>/api/v1/capture/briefing?project=<basename>&source=<source>` with
  `curl --connect-timeout 1 --max-time 2` (token through `-K -`, as everywhere).
- **The output:** it prints the answer's text to stdout. For `SessionStart`, Claude Code adds plain stdout to the context,
  so no JSON escaping is needed in `sh`.
- **Failure:** on any failure, timeout or non-200 answer it prints nothing. It always exits 0.
- **The route** resolves the project from the hint exactly as the transcript route does (§5.2). It answers the `context`
  briefing as `text/plain`, at most 10,000 characters (Claude Code's cap), and `204` when the briefing is off.
- **Two switches turn it off:**
  - the tenant setting `capture.briefing = false`;
  - the line `briefing=off` in `capture.conf`, which saves the round trip.
- **Cost:** up to 2 s at the start of a session when the server is slow or down.

---

## 6. Distillation

### 6.1 Extractive, because Jev only chooses

A distilled memory is **a passage of the session**, chosen and classified by Jev, with light framing added locally.

- **The memory text** is the passage itself, plus a short context line (§6.5). Jev writes nothing; it decides whether the
  passage holds lasting knowledge and of which kind.
- **Why:** Jev answers choice questions only (§2.4), and Jev's own memory cookbook uses the same pattern (a gate question
  over the original text).
- **Cost:** a passage that only makes sense with the conversation around it is dropped, not rewritten. Recall is lower than
  an abstractive summariser's, and precision is higher (R17).

### 6.2 When a session is distilled

A **run** covers the source's chunks from `distill_ord` on, plus the new chunks of its subagent sources. A source becomes
**due** when:

- the session ended (`SessionEnd`, or swept as stale after 24 h). The stale end is the local sweep's, with a final pass
  that closes the trailing turn, or without one for a source it cannot read (no transcript path, or the file is gone),
  and on a server tenant the distill worker's. Any stale end makes the session due only when it, or a subagent of it,
  captured within the last 7 days plus the stale age (8 days by default; for a final pass, the session's last capture
  before that pass counts). An older one waits for `alice capture distill`, so a long downtime, turning distillation
  on or adopting a database makes no backlog (Task 11b and its review). A restored backup makes almost none: a final
  job of more than one pass (over 8 MiB unread), or a subagent whose own final pass captured first, can still mark an
  old open session due, at most the sessions that were open in that backup;
- a `PreCompact` arrived;
- it has been quiet for `distill.idle_minutes` (default 15) and holds at least one undistilled complete turn; or
- someone runs `alice capture distill <session>`.

The **distill worker** is a daemon thread, `alice-distill`. It makes at most one Jev call in flight, at low priority, and
never on the writer or an async runtime thread. A run takes at most 60 turns and 120,000 characters, and a longer backlog
becomes several runs, oldest first.

A run happens only if all of these hold, and otherwise the source's `distill_state` says why:

- `distill.provider = jev` and `judge.provider = jev`. The judge is what removes duplicates, so distilling without it is
  refused (R14);
- a Jev key is set;
- the project is allowed by `cloud_allowed`, asked by the Jev client before the call as for every Jev call;
- the project is not excluded, and the tenant client did not send `no_distill`;
- the breaker is closed and the budget allows the call.

### 6.3 Candidates

The distiller parses the run's chunks by their tags (§3.3) and proposes candidates in this order of priority:

1. **Compaction summaries:** `[summary]` blocks, cut at their headings and bullet groups. "Errors and fixes", "Key
   decisions" and "Pending tasks" sections yield the best candidates. Claude Code writes the summary as a `user` record
   with `isCompactSummary` (and `isVisibleInTranscriptOnly`) whose content is one string: a short preamble, `Summary:`,
   then sections headed `N. **Heading words:**` at the margin, with sub-bullets indented three spaces and nested
   deeper. A heading is `# …`, a bold line, `N. Heading:` or `N. **Heading:**` (Task 12b: the last form was not read
   as a heading, so a real summary went out as blind 1,200-character windows across its sections). Each section is a
   candidate of its own, led by its heading; a section over 1,200 characters is cut between its paragraphs and
   sub-bullet groups, as many whole groups in each part as fit, each part led by the heading again, and a label line
   in it that heads list items of its own (`Pending tasks:`) leads those items instead. What comes before the first
   heading is no candidate when it is the preamble: at most 300 characters (`Summary:` and a sentence or two), or text
   that opens "This session is being continued from a previous conversation". Longer text there is a candidate, and a
   summary without headings is cut as any block is. Where the renderer cut a summary into chunks
   it trimmed the next line's indentation (§3.3); in an indented list such a line stays in the item it was cut from.
2. **Turn conclusions:** the last `[assistant]` block of each turn, cut into paragraphs or bullet groups of 80–1,200
   characters. Code blocks of more than 10 lines are cut out and replaced by `[code]`.
3. **User statements:** `[user]` prompts of 20–800 characters. This is where preferences and decisions the user states come
   from.
4. **Subagent reports:** `[subagent …]` final reports, cut like conclusions. The main session's copy of a report (the
   subagent tool's result) and the subagent's own last text are both subagent reports: they rank below the main
   session's passages and count toward the subagent caps (§6.4). A subagent report is never offered `handoff`: the
   session's handoff is the main session's own text (Task 12b review 2).

Three local filters run before Jev is asked:

- A candidate that holds `[withheld:`, or that is mostly a tool line, is dropped.
- A candidate whose text is nearly contained in an **already saved** item of the run is dropped. Already-saved items are
  the titles and first lines of the session's `[tool alice remember]`, `mempalace_add_drawer` and `mempalace_diary_write`
  lines; "nearly contained" means a trigram overlap ≥ 0.6.
- At most 40 candidates go into one request, by the priority above and newest first; the rest of the run is not asked
  about (R16).

### 6.4 The Jev request

There is one request per run, through the existing client (`Judge::ask_distill`: `JevJudge`'s `ask`, billed to
`jev:distill` and reserved against the distillation share, §6.7). The project slug is passed, so the cloud gate, the
redaction of every covered project, caching by body hash, the rate limit and the budget all apply as they are.

- **`state`:** at most 12,000 characters, newest kept. It holds:
  - `Project: <slug>`;
  - an excerpt of the run: each user prompt (≤ 300 characters) and each turn's final assistant text (≤ 600 characters);
  - `Already saved in this session:` followed by up to 20 already-saved titles.
- **One `choice` question per candidate.** Its instructions read: "Passage <i> from this session (the user had asked:
  «<first sentence of the turn's prompt>»): «<passage>». What lasting knowledge, if any, does it hold for future work?"
  Numbers are verbalised, and options are shuffled with the request-derived seed, as the client already does. The
  options, with their descriptions:

| Option | Description given to Jev | Result |
|---|---|---|
| `decision` | a choice that was made, with its reason, that should guide later work | kind `decision` |
| `bugfix` | a bug that was found: what went wrong, why, and how it was fixed | kind `bugfix` |
| `howto` | a procedure or command sequence that worked and can be reused | kind `howto` |
| `warning` | a pitfall, limit or constraint to avoid in future work | kind `warning` |
| `preference_user` | a preference of this person that holds in every project (tools, style, language) | kind `preference`, project `global` (but see the routing guard, §6.5); the judge also compares it with the session's project (§6.6) |
| `preference_project` | a rule or convention for this repository only | kind `preference`, this project |
| `fact` | a lasting fact about the project, its systems or its environment | kind `fact` |
| `handoff` | where the work stands: what is done and what comes next (offered only for the last two turns of a run that ends with the session, by `session-end`, a compaction or the 24-hour stale end, and never for a subagent report) | kind `handoff` |
| `already_saved` | the same knowledge as an item listed under "Already saved in this session", even when worded differently or with more detail | dropped |
| `ephemeral` | a question, a request or thanks to the assistant, or a pick among choices it offered, with no lasting rule or fact in it; progress narration; a plan for the next minutes; a greeting; a verdict or status report on one task (a review's "approved", "addressed" or "no findings", a checkmark, a count of passing or failing tests, a "done", "fixed" or "pushed" note) that states no lesson for later work; or a result that matters only right now | dropped |
| `unclear` | cannot be understood without the surrounding conversation | dropped |

The option descriptions above are the ones the code sends: Task 13's tuned wording (`already_saved`, and the
questions, picks and thanks of `ephemeral`) and Task 12b's (verdicts and status reports in `ephemeral`; the two
preference options). A lesson stated inside a verdict is still knowledge: `ephemeral` covers only a report that states
no lesson.

- **Keeping a candidate:** it is kept when its most probable option is a knowledge option with probability ≥
  `distill.min_confidence` (default 0.7).
- **Ranking (Task 12b).** The candidates kept by that rule are ranked by priority, then probability, then session
  order: a main-session passage before a subagent report; among equals, handoff, decision, bugfix, warning, fact,
  howto, then preference. Every cap below takes from the top of that order, so a verdict that Jev scores 0.99 never
  displaces a fact of the main session. The handoff comes first because there is at most one per run, offered at the
  session's end, a compaction or the 24-hour stale end, and it is what continuing the work needs most: within a run the
  caps never crowd it out, across runs it has its own slot (below), and it still counts toward the caps.
- **Caps.** One run keeps at most `distill.max_per_run` (default 8), and at most one `handoff`. Of them, at most
  `distill.max_per_subagent_run` (default 2) are subagent reports.
- **The session's caps (Task 12b).** One session keeps at most `distill.max_per_session` (default 16) memories over all
  its runs: its main source and its subagent sources together.
  - **Counting.** The worker counts the session's live (`origin = distill`, not trashed) memories by their author:
    `write_memory` sets the author's agent and session from the source, and a subagent's source carries its session's
    agent and id, so the count holds the subagents' memories too. Provenance is not used: raw retention deletes it with
    the chunks, which would give a capped session fresh room. Among them, those whose idempotency key starts with
    `distill:sub:` came from subagent reports (the main session's copy of a report or the subagent's own; the prefix is
    the only mark a stored memory carries of that, so there is no new column and no migration; §6.5), and those of kind
    `handoff` are handoffs.
  - **The handoff slot (the controller's ruling, Task 12b review).** One slot of `max_per_session` is kept for a
    handoff: memories that are not a handoff fill at most `max_per_session − 1`. The session's first live handoff
    holds the slot; a further handoff, kept while the session had room, takes a slot like any other memory. The total
    never passes `max_per_session`.
  - **The subagents' share.** Subagent reports of the whole session keep at most half of `distill.max_per_session`,
    rounded down (8 of 16), so the main session always has room.
  - **One run** keeps at most `min(max_per_run, what the session has left)`, the free handoff slot included; of them,
    at most `min(max_per_run, what is left of the max_per_session − 1)` are not a handoff, and at most
    `min(max_per_subagent_run, what the subagents' share has left)` are subagent reports.
- **Nothing is paid for what cannot be kept.**
  - A run whose session has no room left, its handoff slot held too, makes no Jev call: its cursors move to the end of
    what is written, the run is recorded, and the source settles `done` (a later due-mark does the same).
  - A run whose session has no room left but a free handoff slot asks about its handoff alone. Only a closing run
    (`session-end`, `pre-compact`, or the 24-hour stale end, which marks the source ended) has candidates for it: it
    makes one small request over its last two turns, main-session passages only, with `handoff` as the only knowledge
    option (`already_saved`, `ephemeral` and `unclear` stay, so a handoff the agent already saved is not stored
    again), and keeps at most one. Any other run of that session makes no call. Once a live distilled handoff of the
    session holds the slot, its capped closing runs make no call either.
  - A run whose subagents' share is used up does not ask about subagent reports.
  - **The consequence.** A long session keeps memories from its first runs only. Once the session's cap is full, the
    rest of it is not asked about, compaction summaries included, however much lasting knowledge it holds. On the
    owner's real sessions that was about half of each long main transcript and nearly all subagent text
    (`docs/benchmarks/sp2.md`).
- **Cost:** about 20,000 input tokens for a 40-candidate request, about $0.0008. A typical session costs one or two runs.

### 6.5 From an answer to a memory

Each kept candidate is written through the service's `remember` path, so redaction, exact dedupe and the judge pipeline all
apply:

| Field | Value |
|---|---|
| `kind` | from the option. `kind_explicit = 1`, so the judge never reclassifies it |
| `project` | the source's project, or `global` for `preference_user`. **Routing guard (Task 12b):** a `preference_user` passage that names the project stays in it: its slug, its name or its working tree's folder name as a whole word (in any case, names of three or more characters), or a path under that folder |
| `title` | the passage's first sentence, at most 120 characters (cut at a word boundary). When that is a table header, or 25 characters or fewer (a bare heading, a label), the title is the passage's first sentence after its head lines (headings, table rows, short lines that end no sentence), run on into the next sentence of its line while it has 25 characters or fewer, at most 80 characters, cut at a word with `…`; a passage that is only a table is titled by its first row. A heading longer than 25 characters that is not a table header stays the title |
| `body` | the passage, at most 1,200 characters. For conclusions and subagent reports, a last line `Context: <first sentence of the turn's prompt>` is added when that prompt has 4 or more words, so the memory stands on its own |
| `importance` | `distill.importance` (default 2, allowed 1–3). Distilled memories never reach the briefing's key-memory sections, which need 4, and rank below explicit memories, which default to 3 |
| `topics` | none |
| `author` | `{agent: <source agent>, session: <session id>, model: null}` |
| `origin` | `distill` (new column, §8.1) |
| `idempotency_key` | `distill:` + the first 32 hex characters of SHA-256(source uri ‖ passage text), so a re-run returns the first memory; `distill:sub:` + the same for a subagent report, which is how the session's caps count them |
| provenance | the ids of the chunks the passage and its context line came from (the first writer of the existing `provenance` table) |
| event actor | `distill:jev` (a new `Actor` variant) |

After the memories are written, `distill_ord` moves to the end of the run. A crash in between repeats the run. The Jev
call then hits the cache (same body) and the memories come back as duplicates through their idempotency keys, unless
the memories written before the crash changed the session's caps (§6.4): then the request can differ (subagent reports
or all but the handoff are no longer asked about) and one more request is paid, or none is sent when the session is
full; the memories still come back as duplicates.

### 6.6 No floods and no doubles: what changes in the judge and the briefing

**Duplicates of what the agent saved.** A distilled memory is written after the session's own `remember`s and gets a
higher `seq`, so the existing judge compares it with them: same project, the five nearest plus title matches, older and
active. A duplicate at or above `judge.auto_apply_threshold` is merged as today.

**A preference filed under `global` meets its session's project (Task 11b).** A `preference_user` passage is stored in
`global` (§6.4), where the agent's own copy of it, saved in the session's project, is not. So a distilled memory in
`global` is also compared with the memories of its **source project**: the project of the chunks in its provenance, or,
once raw retention removed them, the project of its author session's capture source. The same limits apply there (the
five nearest plus title matches, older and active), and the newcomer's relations to both sets are judged together.

- **Cloud.** The comparison then covers both projects: the one gate asks `cloud_allowed` for each and redacts with each
  project's rules.
- **A denied source project is left out.** None of its memories is read into the comparison, so none is sent, and the
  comparison with `global`'s memories still goes to Jev. Comparing them through the heuristic only would decide nothing
  either: it compares memories of one project only.
- **Across projects, a match only ever settles as a duplicate or a link** (Task 11b review). A duplicate of a source
  project's memory, or an update of an explicit one there, trashes the `global` newcomer as its duplicate, so a
  user-wide preference the project already holds lives on in the project only. An update of a distilled memory there
  is linked `relates_to` and never supersedes it: a user-wide memory never retires a project's own rule, and the
  outcome does not depend on which of the two came first (a project's newcomer never meets `global`).
- An agent's or a person's memory in `global` meets `global`'s memories only, as before.

**Hidden until judged.** Today `pending` and `active` are one state for search and the briefing. A distilled memory is
different: recall and `context` skip it until its judge stage is over (`judge_state = done`, which includes giving up
after five attempts). A near-copy of an explicit memory therefore never shows as a double while the judge works or is
unreachable. It is one more condition on the memory candidates, `origin = 'distill' AND judge_state <> 'done'` excluded,
and `get_memory` by id still returns it.

**Distilled memories never ask a person.** These rules are new in the judge's apply step, for a newcomer with
`origin = distill`:

| Judge result | Today | For a distilled newcomer |
|---|---|---|
| duplicate ≥ auto | newcomer trashed as `duplicate_of` the older | the same |
| duplicate between review and auto (would be a `merge` review) | review item | newcomer trashed as `duplicate_of`, no review item |
| updates ≥ auto | newcomer supersedes the older | the same if the older one is also distilled and in the same project (in another project, a `relates_to` link). If the older one was written by an agent or a person (or imported), the newcomer is trashed as `duplicate_of` it, no review item (Task 11b) |
| updates between review and auto | `supersede` review | of a distilled memory: `relates_to` link, no review item. Of an agent's or a person's: trashed as `duplicate_of` it, no review item (Task 11b) |
| contradicts ≥ review | link plus `contradiction` review | `contradicts` link, no review item |
| heuristic stand-in (Jev unreachable) | review-only suggestions | nothing; the memory waits for the judge's retry |

Only the strongest relation to each memory counts (an update that a contradiction outweighs is the contradiction's
link), and the best match settles it: the newcomer goes to the trash as the duplicate of the memory it duplicates, or of
the explicit memory it updates, with the highest probability at or above `judge.review_threshold`, and nothing else is
applied. **A distilled update of an explicit memory is its duplicate (Task 11b).** It is a near-copy of what an agent or
a person saved, with more detail or a newer value; the explicit memory is kept, and the passage stays in the raw
capture: a distilled memory never replaces an explicit one (R19). Such updates,
kept as linked pairs, and preferences filed under `global` were the two ways the Task 13 evaluation found for a
distilled copy of a saved memory to stay live. The heuristic and its stand-in still apply no update (Task 11's R2): the
heuristic applies only the same text, and a stand-in nothing.

**An explicit memory beats a distilled one.** An agent's or person's newcomer that duplicates an **older distilled**
memory survives, and the distilled one is trashed as its duplicate. This reverses today's "older survives" for that pair,
so explicit memories are never lost to machine copies.

**Maintenance and the briefing:**
- `merge_suggestions` skips distilled memories, as it skips diaries.
- The **briefing** marks distilled items `(auto)`.
- "Recent decisions" lists explicit decisions first and fills the remaining slots with distilled ones.
- The key-memories fallback (importance ≥ 3) excludes distilled memories.
- "Last handoff" stays the newest handoff, distilled or not. A session's own end is the freshest answer to "where was I",
  and Jev keeps only a high-confidence one.

**Recall** shows `(auto)` in the text format and `origin: "distill"` in JSON.

All of these rules are tested (§11).

### 6.7 Cost, budget, breaker

- **Budget share.** Distillation spends from `judge.daily_budget_usd`, the judge's own budget.
  - It may use at most `distill.budget_share` of it (default 0.5), so it can never starve memory judging or precise recall.
  - The ledger gets a second provider row, `jev:distill`.
  - The judge's day total becomes `jev` plus `jev:distill` **everywhere it is read**: the budget reservation's `spent`
    (`store/judge.rs`), stats' `cost_today_usd` and `cost_7d_usd`, and the doctor's 80 % warning. Stats also show the
    distill part on its own.
  - A distill call reserves against both the total and its share, by the existing reservation rule.
  - When the share is used up, the source waits for the next UTC day (`distill_state = waiting_budget`). This is not a
    failure.
- **Breaker.** Three distill calls in a row that fail (unreachable, `Failed`, or a timeout after the client's retries) open
  the breaker for 15 minutes, doubling at each reopening up to 6 hours. A success closes it. `Budget` and `Denied` are
  answers, not failures.
- **A denied project** is never sent. Its source becomes `skipped_cloud` and is due again when the cloud policy allows it;
  the policy is re-checked every five minutes, as for held-back embeddings.
- **Raw capture never waits on any of this.**

### 6.8 Privacy summary

| What | Where it goes |
|---|---|
| Raw chunks | Stored locally (or in the tenant), redacted. They reach a remote embedder only if the cloud policy allows, as today |
| The distill request | Redacted with every covered project's rules and sent only for allowed projects, through the one cloud gate (`SettingsHandle::cloud_allowed`) |
| Thinking blocks, images, file contents of Read and Edit, and memory-tool results | Never stored |
| Turns of projects marked `no_capture` | Never stored |

---

## 7. Settings

`[capture]` and `[distill]` exist as stubs in `settings.rs` (`CaptureSettings` with six agent flags; `DistillSettings
{provider, model}` as strings). SP2 fills them in:

```toml
[capture]
enabled = true              # master switch; hooks still run but the daemon stores nothing when off
claude_code = true          # existing per-agent flags (cursor, windsurf, opencode, gemini stay for later readers)
codex = true
excluded_projects = []      # never captured, neither raw nor distilled
include_subagents = true
include_thinking = false
tool_output_chars = 800     # 0–8,000: head 5/8, tail 3/8
chunk_chars = 2000          # 500–8,000
briefing = true             # the SessionStart briefing (local hook and tenant script)
raw_retention = "forever"   # captured transcripts only (§3.9); same syntax and limits as storage.raw_retention;
                            # tenants: 180d, pinned by ALICE_CAPTURE_RAW_RETENTION in the tenant's env file

[distill]
provider = "off"            # "off" | "jev"   (the existing string becomes an enum)
# model                     # existing optional string, reserved for later providers; ignored with "jev" (judge.jev.model is used)
min_confidence = 0.7        # 0.5–0.99
max_per_run = 8             # 1–50
max_per_session = 16        # 2–200: all runs of one session, its subagents included; subagents keep at most half
                            # effective per run: min(max_per_run, what is left of it); one slot is a handoff's
max_per_subagent_run = 2    # 0–50: subagent reports kept in one run
                            # effective: min(this, max_per_run, what is left of max_per_session / 2)
idle_minutes = 15           # 1–1,440
importance = 2              # 1–3
budget_share = 0.5          # 0.05–1.0 of judge.daily_budget_usd
```

**Loosening.** These join `loosened_protections`, so a change to them in `config.toml` needs three equal polls, and an API
change is announced and named in the `settings.change` event:
- `capture.enabled` off → on;
- a `capture.<agent>` flag off → on;
- an entry removed from `capture.excluded_projects`;
- `capture.include_thinking` off → on;
- `distill.provider` to `jev`;
- `distill.budget_share` raised;
- the project scope's `no_capture` removed (through the project file's existing gate).

**Project scope.** `.alice.toml` `[privacy]` gains `no_capture = true`. It can only tighten. A broken project file fails
closed, which here means no capture.

**Defaults.**
- `distill.provider` defaults to `off`, because sending session text to a cloud model is opt-in for an open-source tool
  (R15). The owner turns it on (`alice config set distill.provider jev`); on a server the admin turns it on per tenant
  (`alice-server distill on <name>|--all`, §9.3).
- `capture.enabled` defaults to on, because installing the hooks is the opt-in.

**Tenants.** No new setting names a URL or a path, so tenant validation (server spec §2.2 item 6) gains no entry. The
local-only notify endpoint is refused in tenant mode instead.

---

## 8. Data model, API and CLI changes

### 8.1 Migration (the next free number; `0007_capture.sql` today)

```sql
CREATE TABLE capture_state(
  source_id        TEXT PRIMARY KEY REFERENCES sources(id) ON DELETE CASCADE,
  agent            TEXT NOT NULL,
  session_id       TEXT NOT NULL,
  parent_source_id TEXT,                       -- a subagent's main session
  transcript_path  TEXT,                       -- local only; NULL in tenant mode
  file_key         TEXT,                       -- local only: dev:inode:sha256(first line)
  byte_offset      INTEGER NOT NULL DEFAULT 0,
  next_ord         INTEGER NOT NULL DEFAULT 0,
  last_event       TEXT,
  last_capture_at  TEXT,
  ended            INTEGER NOT NULL DEFAULT 0,
  no_distill       INTEGER NOT NULL DEFAULT 0, -- tenant client flag
  gaps             INTEGER NOT NULL DEFAULT 0, -- tenant forward gaps accepted
  distill_ord      INTEGER NOT NULL DEFAULT 0, -- chunks with ord < this were distilled
  distill_state    TEXT NOT NULL DEFAULT 'idle',
      -- idle|due|done|waiting_budget|waiting_judge|skipped_cloud|skipped_excluded|skipped_backfill|failed
  distill_attempts INTEGER NOT NULL DEFAULT 0,
  distill_next_try_at TEXT,
  created_at       TEXT NOT NULL
);
CREATE INDEX capture_state_distill ON capture_state(distill_state, distill_next_try_at);
CREATE INDEX capture_state_session ON capture_state(agent, session_id);

CREATE TABLE capture_turns(
  agent TEXT NOT NULL, turn_key TEXT NOT NULL, source_id TEXT NOT NULL, first_ord INTEGER NOT NULL,
  PRIMARY KEY(agent, turn_key)
) WITHOUT ROWID;                               -- kept when a source is deleted (§3.5)

ALTER TABLE memories ADD COLUMN origin TEXT NOT NULL DEFAULT 'agent';   -- agent | distill | import
UPDATE memories SET origin = 'import' WHERE judge_state = 'skipped';    -- M3 imports
CREATE INDEX sources_session ON sources(session_id) WHERE session_id IS NOT NULL;
```

- **Size:** `capture_turns` grows by about one row per turn, ~180,000 rows (~18 MB) a year for the owner.
- **The importer** sets `origin = 'import'` on new imports.

**Writer operations:**
- `CaptureSlice` (§3.8);
- `CaptureStateUpdate`, for distill state and registration;
- `Remember`, which gains two internal, non-wire fields: `origin` and `provenance` (chunk ids, inserted with the memory);
- the judge's apply step reads `origin` (§6.6);
- `JudgeCall` gains the consumer tag that selects the ledger row (`jev` or `jev:distill`).

### 8.2 HTTP

All routes are behind the existing token, Host and Origin guard, except the two public scripts.

| Route | Purpose |
|---|---|
| `POST /api/v1/capture/notify` | Hook notification (local only; 400 in tenant mode) → `202 {queued}` |
| `POST /api/v1/capture/transcript` | Tenant byte upload (§5.2); its own 8 MiB body limit |
| `GET /api/v1/capture/status` | Recent sessions (50), cursors, distill states and counters; also `{tenant: bool}`, which setup uses to check the token |
| `POST /api/v1/capture/distill` | `{session_id}` or `{source_id}`, marks it due now |
| `GET /api/v1/capture/briefing?project=…&source=…` | The SessionStart briefing as `text/plain` (≤ 10,000 characters); `204` when `capture.briefing` is off or `source` is `resume` or `fork`. The project resolves from `cwd` locally (the hook sends it as a query parameter) and from the hint on a tenant (§5.7) |
| `GET /api/v1/capture/setup.sh`, `GET /api/v1/capture/hook.sh` | Public static scripts (`text/plain; charset=utf-8`, `Cache-Control: no-store`); the hook script's version line is stamped with a hash of its text (§5.3) |

- **The spool** gains `SpoolEntry::CaptureNotify`.
- **No new MCP tool.** Transcripts are reached through `recall` with `include_raw`, as today.

### 8.3 CLI

```
alice hook <session-start|stop|subagent-stop|pre-compact|session-end> [--agent claude-code|codex]
alice hooks install|uninstall|status <claude|codex> [--settings <path>] [--dry-run]
alice capture status [--json]
alice capture backfill <claude|codex> [--since d] [--project p] [--distill]
alice capture distill <session-id>
```

### 8.4 Health, doctor, stats

- **`Health.capture`** (absent when capture never ran):
  - `sessions_24h`, `chunks_24h`, `last_capture_at`, `open_sessions`;
  - `distill_provider`;
  - `distill_state`: `off`, `ready`, `waiting_judge`, `waiting_budget`, `breaker_open` or `no_key`;
  - `distill_due`, `distilled_24h`, `distill_dropped_24h` (duplicates and uncertain ones);
  - `distill_warning`.
- **`alice doctor`** gains two lines, pushed in `health_checks` and `daemon_checks` as the existing lines are:
  - **`capture`:** OK with the last capture time. Warn if installed hooks point at a missing binary, if hooks are
    installed but nothing was captured for 7 days, or if Codex transcript lines were not recognised.
  - **`distill`:** off, OK, or a Warn with its fix (set the judge to Jev, set a key, the breaker is open until …, the share
    is used up today).
  The doctor reads `settings.json` and `hooks.json` read-only, to see whether hooks are installed.
- **`alice stats`** gains `distilled_today`, `distill_dropped_today` and `distill_cost_today_usd`; `cost_today_usd` stays
  the total.

---

## 9. Migration from MemPalace

### 9.1 The owner's cut-over

Today, MemPalace's Claude Code plugin hooks mine transcripts and ask for checkpoint diaries. A SessionStart stopgap
(`~/.local/bin/alice-session-start`) prints `alice context`, and at most every 12 hours it runs `alice import mempalace`.
The cut-over, in this order:

1. **Install SP2:** `alice hooks install claude`. The briefing now comes from `alice hook session-start`, and
   transcripts are captured.
2. **Run `alice import mempalace` one last time,** so everything MemPalace caught up to now is in Alice.
3. **Remove the stopgap** from SessionStart, with `/hooks` or by hand; the installer lists it (§4.3). This also ends the
   12-hourly import.
4. **Disable the MemPalace plugin** (`/plugin`, or `claude plugin disable …`). That stops its hooks and its MCP server.
   `compat.mempalace` stays on, so prompts that still call `mempalace_*` tools keep working against Alice.
5. **Optionally trim the "MemPalace Protocol" blocks** in CLAUDE.md files (diary after every 5 tool uses). Capture makes
   them redundant, and explicit `remember`s of decisions and fixes remain the better memories. This is the owner's call.
6. **Turn distillation on:** `alice config set distill.provider jev` (the judge is already `jev`). This step is for the
   owner's local install until the owner moves to the server (§9.4). After the move, the tenant's settings decide (§9.3).

### 9.2 No duplicate transcripts

M3 imports a MemPalace transcript as a source with `uri = file://<path>/<session-id>.jsonl` and
`session_id = <session-id>`. MemPalace's chunking cannot be aligned with SP2's turns, so the rule is that **a session is held
by one side**:

- **SP2 captures a session MemPalace already mined.** On the first capture of session *S*, if an imported source with
  `session_id = S` exists, SP2 captures only turns whose first record's timestamp is later than that source's `ended_at`.
  That `ended_at` is the latest filing time of its drawers, and MemPalace mined the whole file each time it filed.
  - A turn with no timestamp is captured.
  - An imported source with no `ended_at` is no evidence, so everything is captured.
  - The sessions active during the cut-over are the only overlap, and it is bounded by minutes (R18).
- **The importer meets a session SP2 already captured.** `alice import mempalace` skips transcript drawers whose
  `session_id` is held by an SP2 capture source (a small change in M3's raw-session tier). In either order, a session's
  turns are held once.
- **Checkpoint diaries.** The imported checkpoint chunks (`mempalace-checkpoint:<wing>:<date>`, agent
  `mempalace checkpoint`) are the agent's own summaries, not transcript text, so they do not collide with SP2's chunks. They
  stay in the raw tier, are never distilled (the distiller reads capture sources only), and stop growing at the cut-over.

### 9.3 Tenants

- A team member's palace is imported by the admin (server spec §2.9). The importer's skip rule above applies there too.
- They have no MemPalace hooks to remove, only the setup line to run.
- **Jev is on for every tenant, with one shared key the owner manages** (the owner's decision, 2026-10-16).
  - The product default in the open-source code stays `distill.provider = off` (R15). On this deployment the admin turns
    distillation on, **per tenant** (controller ruling, 2026-10-17, R28):
    - `alice-server jev-key set` (and a rotation) changes **only the key**; it never touches a tenant's `judge.provider`
      or `distill.provider`.
    - Once the key is provisioned (server-mode addendum 5), `alice-server add` gives each new tenant `judge.provider = jev`
      and takes `distill.provider` from `/etc/alice/distill.default` (one line, `on` or `off`; `off` when the file is
      missing; any other content is refused before anything is created).
    - `alice-server distill on|off <name>|--all` switches a tenant's distillation, and `alice-server distill status`
      shows each tenant's setting and the default.
  - How the key reaches each daemon is that lane's design. SP2 only needs the judge's existing key lookup
    (`jev_api_key`) to find it.
  - A person never sees or supplies the key. The setup script never asks for one. Its last lines say whether
    distillation is on for that tenant (from `/capture/status`).
- **Spend is per tenant,** within each tenant's own `judge.daily_budget_usd` (default $1.00), of which distillation may use
  `distill.budget_share` (default 0.5).
  - **The deployment-wide ceiling** is 5 daemons (four people and the team store) × the per-tenant budget: **$5.00 a day
    at the default**, of which distillation can be at most 4 × $0.50 = $2.00.
  - **Expected spend** is far lower: about $0.001–0.003 per distilled session (§6.4), a few cents a day per person.
  - The owner changes a tenant's budget with `alice-server cli <name> config set judge.daily_budget_usd <usd>`. Raising it
    counts as a loosening and is named in the tenant's history.
- **Retention:** captured transcripts age out after 180 days on tenants (§3.9). `alice-server` pins it (§9.5). Imported
  history is not affected.

### 9.4 The owner becomes a tenant (adoption)

Server-mode addendum 4 (`alice-server adopt`) moves the owner's Mac database onto their tenant after the reviews. If SP2
ran locally before the move, the variant goes:

1. **Before the move,** on the Mac: let the local capture finish (the session in progress ends, or the daemon has swept).
   `alice import mempalace` runs one last time if MemPalace is still active.
2. **The admin adopts the database** (pane 3).
3. **On the Mac:** run `alice hooks uninstall claude`, which removes the local hooks (backup kept). Also remove the
   stopgap SessionStart script, if it is still there. If SP2 never ran locally, there is nothing to uninstall and the local
   hooks are never installed.
4. **Run the tenant setup line** (§5.4) instead of `alice hooks install claude`. Then disable the MemPalace plugin and trim
   the CLAUDE.md protocol blocks, as in §9.1 steps 4–5.

**No duplicates across the move:**
- **The tables travel.** `capture_turns` and `capture_state` live in `alice.db`, so they move with it.
- **First upload from 0.** The tenant script's first upload for a session the Mac already captured starts at offset 0. The
  server's cursor for that source is ahead, so it answers `409` with that cursor, and the script jumps there. It is the
  same file on the same Mac, so the byte offsets agree.
- **Fallback.** Any resent turn is skipped by its turn key (R2). No locally captured turn is stored twice.
- **Local paths stay unread.** In tenant mode the capture worker never reads files: notify is refused and sweeps of file
  paths do not run. An adopted row's `transcript_path` and `file_key` are ignored. A source left open by the move is marked
  ended after 24 hours without events and distilled as usual, without a final file pass.
- **Tested** by capturing into a local home, starting a `--tenant` daemon on the same database, and uploading the same
  transcript from offset 0. The test checks the 409 jump, that no turn is stored again, and that the next real turn is
  stored once (§11.1).

### 9.5 Interfaces the server lane implements

SP2 needs these from `deploy/server/alice-server` (pane 3). SP2 does not edit that script.

| Interface | What it does |
|---|---|
| Print the setup line | `alice-server add <name>` (and a command to print it again) prints `curl -fsSL https://<host>/u/<name>/api/v1/capture/setup.sh \| sh -s -- https://<host>/u/<name>` |
| Pin capture retention | `add` writes `ALICE_CAPTURE_RAW_RETENTION=180d` into the tenant's env file, and `upgrade` adds it where unset. The environment wins over `config.toml`, as `ALICE_BACKUP_DAILY` does (server spec §2.7), so a tenant's own patch cannot loosen it; the admin changes the line |
| Jev on with the shared key | When `alice-server jev-key set` has provisioned the key, `add` sets `judge.provider = jev` for the tenant and `distill.provider` from `/etc/alice/distill.default` (`off` without it); `alice-server distill on\|off <name>\|--all` and `distill status` switch and show it; `jev-key set` changes only the key (R28) |
| Adoption | `alice-server adopt` (addendum 4) moves the database whole, so `capture_state` and `capture_turns` travel with it (§9.4) |

---

## 10. Failure modes

| Failure | Behaviour |
|---|---|
| Daemon down when a hook fires | The notify is spooled and the daemon is started detached. The hook exits 0 within its budget, and the drained notify triggers a pass from the cursor |
| Daemon killed mid-pass | The cursor and turn keys commit with each turn's last slice. The next pass re-renders the open turn identically, and `(source, ord)` drops the repeats |
| Transcript lags at `Stop` | 1.5 s settle (2 s in the tenant script). A message that still arrives late is captured at the next pass |
| Agent crashes (no `SessionEnd`) | The sweep marks the source ended after 24 h and makes a final pass. A source it cannot read (no transcript path, or the file is gone) is ended without a final pass, as a server tenant's distill worker ends a source with no capture for 24 h. Any of these stale ends makes the session due for distillation when it captured within the last 7 days plus the stale age (8 days by default); an older one waits for `alice capture distill`, so a long downtime makes no backlog (Task 11b and its review). After a restored backup, a final job of more than one pass (over 8 MiB unread), or a subagent whose own final pass captured first, can still mark an old open session due, at most the sessions that were open in that backup |
| Unknown or malformed line | Skipped and counted. A turn with only skipped lines leaves no chunk. `alice doctor` warns when a reader skipped a large share of a file (format drift) |
| Line over 4 MiB | Skipped with a stub in its turn |
| Transcript file replaced or truncated | The cursor resets to 0; turn dedupe skips stored turns |
| Continued or forked session copying old records | Copied turns skipped by turn key |
| Settings say capture off, agent off, or project excluded | Nothing stored; the cursor moves past those turns |
| Block mode and a secret in a turn | That chunk becomes `[withheld: …]` and capture continues |
| Tenant unreachable | The script keeps its offset and the next hook retries. `SessionStart` sweeps up to 5 unfinished sessions |
| Tenant token rotated | `401` → the script logs once and pauses 1 h; writing the new token into `~/.config/alice/token` (or re-running the setup line, which asks) fixes it |
| Tenant slow or down at `SessionStart` | The briefing call gives up after 2 s (1 s to connect) and prints nothing; the session starts without it and the MCP instructions still ask for `context` |
| Capture retention reached | Old captured chunks are deleted; imported history, cursors, turn keys and memories stay |
| Owner's database adopted into a tenant | The tenant script's offset-0 upload gets a `409` jump; resent turns are skipped by key; local paths in adopted rows are ignored |
| Tenant client lost its offset | `409` with the server's cursor; at most one wasted upload |
| Tenant server restored to an older copy | A forward offset is accepted as a gap and counted |
| Jev unreachable or failing | Distill breaker (15 min doubling to 6 h); raw capture continues; `alice doctor` shows it |
| Distill share used up | Waits for the next UTC day; judge and precise recall keep their part of the budget |
| `judge.provider` not `jev` | Distillation waits (`waiting_judge`) with a doctor fix line |
| Project denied the cloud | Raw only; `skipped_cloud`; due again when the policy allows |
| Hooks point at a moved binary | The hook command fails non-blocking (Claude Code shows one stderr line); `alice doctor` warns; re-running `alice hooks install claude` fixes it |
| Malformed `settings.json` | `alice hooks install` refuses and changes nothing |
| Codex `transcript_path` null and no rollout found | Nothing captured; counted for `alice doctor` |
| Codex `.jsonl.zst` | Skipped and counted (v1) |

---

## 11. Testing and quality

All tests run with `env -u TYPESAFE_API_KEY -u ALICE_EMBEDDING_API_KEY`. Secret-shaped strings are built with `concat!`.
Tests use a throwaway `ALICE_HOME` and a throwaway HOME (`HOME`, `CLAUDE_CONFIG_DIR` and `CODEX_HOME` pointed into a
temporary directory), never the real ones.

### 11.1 Unit and integration tests

**Structure-only samples first** (the controller's permission, with its conditions). At the start of execution:
1. Copy at most two Claude Code transcripts, and one Codex rollout if one exists, read-only into the lane's scratch
   directory.
2. Commit only **structure-only** fixtures made from them: every text value replaced, and ids, timestamps, paths and the
   project name rewritten.
3. Before committing, run a one-off check. No substring of 16 or more characters from the copies' text values may appear
   in the fixtures, and `grep -c -F -- "$TYPESAFE_API_KEY"` on each fixture must print 0.
4. Delete the copies. Record the check's result in the lane's ledger.
If there is no Codex rollout, Codex stays seam-only until a sample exists (§13).

**Reader and renderer:** synthetic Claude Code and Codex JSONL fixtures, committed (the structure-only samples plus
hand-written ones). They cover:
- turn boundaries and completeness;
- every row of the rendering table;
- `system-reminder` stripping;
- memory-tool results omitted;
- images, binaries and thinking dropped;
- streaming duplicates;
- compaction summaries;
- deterministic chunk cuts (render twice, byte-equal);
- oversized lines;
- unknown line types.

**Capture state:**
- incremental passes;
- a pass interrupted after each slice (the writer is stopped between slices), with no duplicate and no gap;
- a replaced or truncated file;
- a continuation file with copied records;
- subagent sources;
- late records of a stored turn becoming its continuation chunks;
- excluded projects (cursor moves, nothing stored, turn keys recorded, so a continuation file stores nothing either);
- block-mode stubs;
- retention deleting a source, after which a re-pass stores nothing;
- `capture.raw_retention` deleting old captured chunks while an imported source of the same age, and other raw sources,
  are untouched; the environment pin wins over a patch;
- **adoption:** capture into a local home, start a `--tenant` daemon on the same database, upload the same transcript from
  offset 0. The upload gets the `409` jump, no turn is stored again, the next real turn is stored once, and the adopted
  rows' local paths are never read.

**`alice hook`:**
- the output JSON for `SessionStart` (`startup`, `clear` and `compact`; none for `resume`);
- nothing printed when the briefing misses the 90 ms deadline;
- exit 0 on every error path;
- spool and detached start when the daemon is down (`ALICE_NO_AUTOSTART` variant);
- **latency:** an ignored release-mode test runs 200 invocations each with the daemon up and with it down, and requires
  p95 < 100 ms; the result goes into `docs/benchmarks/sp2.md`.

**`alice hooks install`:** against a throwaway HOME, covering:
- creating a missing file;
- merging beside existing hooks of the same events;
- idempotence (a second install is byte-identical);
- a moved binary repaired;
- uninstall leaving other hooks intact;
- a malformed file refused untouched;
- backups capped at 5;
- the Codex refusal when `config.toml` has inline hooks.

**Tenant path:**
- `POST /capture/transcript` against a `--tenant` daemon: the offset protocol (equal, lower → 409, higher → gap), partial
  last lines, `oversized_line`, forced redaction, the project-hint placeholder shared with the MCP path, `no_capture` and
  `no_distill`, and the 8 MiB limit;
- the notify endpoint refused in tenant mode;
- the public script routes need no token and hold no secret;
- **the shell scripts run for real** under `sh` on ubuntu and macOS CI, against a local tenant daemon over
  `http://127.0.0.1`, with a fake `claude` command on `PATH` that records its arguments. The checks:
  - setup writes a 0600 conf;
  - the token never appears in any process's argv (a `ps` sampler during the run) or in the log;
  - the plugin files are well formed;
  - a transcript grown in steps is captured once;
  - the `SessionStart` briefing is printed as plain text;
  - nothing is printed within 2 s when the daemon is stopped, or made to stall by a test stand-in;
  - nothing is printed with `briefing=off` or `capture.briefing = false`;
  - uninstall removes what setup wrote.

**Distillation**, with recorded Jev fixtures (request and response bodies only, no headers):
- candidate extraction and its filters;
- the request shape (one question per candidate, the options, `state` within 12,000 characters);
- mapping answers to memories (kind, the `global` routing of `preference_user`, caps, at most one handoff);
- idempotent re-runs;
- provenance rows;
- `origin` and the actor;
- the budget share against the shared total;
- the breaker;
- a denied project never sent (the fake transport records no call);
- `waiting_judge` when the judge is not Jev;
- every row of §6.6's judge table;
- a distilled memory hidden from recall and the briefing until its judge stage is over;
- an explicit newcomer surviving over a distilled duplicate;
- `merge_suggestions` skipping distilled memories;
- the briefing's `(auto)` marks and section rules.

**Migration:**
- SP2's first capture of an imported session takes only turns after the import's end;
- the importer skips drawers of a session SP2 holds;
- `origin = 'import'` is backfilled.

**Settings:**
- each new loosening key goes through the three-poll gate;
- the project `no_capture` key fails closed;
- the validation ranges.

**Gates** (unchanged from M2): the search-quality tests keep passing with unchanged thresholds, and the performance
budgets of core spec §4.6 still hold. A capture pass runs beside the `service_concurrency` test without breaking its bounds.

### 11.2 The synthetic labelled set (committed)

`crates/alice-daemon/tests/fixtures/distill/` holds 30 synthetic Claude Code sessions in the transcript format above,
about a third of them in Thai or mixed Thai and English:

- **20 sessions** with 1–4 gold items each (about 50 in all). Each gold item has the passage it comes from, one or more
  accepted kinds (e.g. `bugfix|howto`) and a key phrase that a correct memory must contain.
- **6 sessions with nothing worth keeping:** chit-chat, progress narration, and plans for the next minutes.
- **4 sessions** where the agent `remember`ed the key items itself through a `remember` tool call, to test that nothing is
  stored twice.

`labels.json` holds the gold. The fixtures are hand-written from scratch, with no owner data. The labels are one author's,
a caveat the results state, as `m2-jev.md` does.

### 11.3 Evaluation protocol and thresholds (declared before any run)

- **Runs.** An ignored live test (`distill_eval`, with the key loaded as the Global Constraints say) runs distillation over
  the set through the real pipeline and the real judge. It writes `docs/benchmarks/sp2-distill.md`. The CI test replays the
  recorded answers, so the pipeline is checked deterministically.
- **Matching.** A stored memory matches a gold item when it contains the item's key phrase and its kind is one of the item's
  accepted kinds.
- **Metrics:**
  - *precision* = matched stored memories / stored memories;
  - *recall* = matched gold items / gold items;
  - *doubles* = gold items of the 4 "already remembered" sessions that end up stored twice;
  - *flood* = memories from the 6 "nothing" sessions.
- **Thresholds:**

| Metric | Threshold |
|---|---|
| Precision | ≥ 0.80 |
| Recall | ≥ 0.60 |
| Doubles | ≤ 10 % of those items |
| Flood | ≤ 2 memories in all |
| Spend | < $0.10 for the whole run |

- **If a threshold fails,** there is one round of changes to the candidate rules, the option wording or
  `distill.min_confidence`, then a single re-run. Both runs are reported, and the controller decides. The set itself and the
  thresholds are not changed after the first run.

### 11.4 The owner's real sessions (local only, before acceptance)

- **Setup.** 3–5 of the owner's Claude Code sessions, chosen with the controller, are **copied** into this lane's scratch
  directory. They are captured and distilled there, with a throwaway `ALICE_HOME` and Jev allowed (the owner authorised Jev
  on their own data).
- **Report** (in the scratch directory, never committed): the sessions' chunk counts, the candidates, the kept memories
  with their kinds and probabilities, the duplicates the judge caught, and the cost. The controller and the owner read the
  memories themselves.
- **Committed:** only aggregate numbers (counts, spend, latency), in `docs/benchmarks/sp2.md`, never text.

---

## 12. Rulings (what — why — cost if wrong)

| # | Ruling | Why | Cost if wrong |
|---|---|---|---|
| R1 | **Locally the hook only notifies; the daemon reads the transcript** (it is the same OS user) | The hook stays far under 100 ms whatever the file size; the cursor lives in the database with the chunks; one reader serves hooks, sweeps and backfill. §8.5's "the hook sends chunks to `/ingest`" is kept in spirit (the daemon writes through the ingest path's rules) but not in transport | A transcript on another machine than the daemon (a container, an SSH host) is not captured; none is in scope |
| R2 | **One turn = the unit of capture and dedupe**; turn key = first record's uuid, committed with the chunks | Exactly-once across hook repeats, crashes, file replacement, continuation copies and tenant resends with one rule | About one row per turn (~18 MB a year) |
| R3 | **The trailing turn waits** until a later turn, a stop-type event plus settle, or 5 quiet minutes | Chunks are cut deterministically only from complete turns; the transcript can lag at `Stop` | The live session's last turn reaches the raw tier one turn late |
| R4 | **Rendering trims hard**: no file contents for Read and Edit, tool output head 500 + tail 300, no thinking, no images, no memory-tool text, `system-reminder` stripped | The raw tier is evidence of what happened, not a code index, and injected context would flood it (and loop the briefing back into it) | A question about exact tool output older than the trim is answered from the head and tail only |
| R5 | **A source owns its project**, fixed at first capture | Settles M1's open re-ingest ruling; a session belongs to one repository | A session that `cd`s into another repository stays in the first project |
| R6 | **Excluded turns are skipped, and the cursor moves past them** | Un-excluding a project must not backfill what the person meant to keep out | Turning capture back on never recovers the excluded period |
| R7 | **Synchronous command hooks with a 10 s guard**, not `async` | Our own budget is under 100 ms; `async` semantics around `SessionEnd` and in older versions are less certain | None known |
| R8 | **A block-mode secret withholds its chunk, not the capture** | One secret must not stop a session's capture forever; nothing with a secret is stored | The withheld chunk's other text is lost |
| R9 | **No `http` hooks for tenants** | No transcript in the payload; the token would sit in settings | — |
| R10 | **No root allowlist for the local notify path** | A local token holder is the same user and can read those files anyway; validation is a regular `.jsonl` file opened read-only | A local process with the token can make the daemon store any readable `.jsonl` file of that user |
| R11 | **Tenants: a Claude Code plugin from a local marketplace written by the setup script**, plus a POSIX `sh` + `curl` hook script | Claude Code merges and removes plugin hooks itself (no JSON tooling on the person's machine); the files come from the tenant at its version | Needs `claude plugin` (with a printed `/plugin` fallback); an update means re-running the setup line |
| R12 | **The tenant script verifies no ping proof** | An HMAC with `openssl` puts the token in argv; TLS with a public certificate pins the host | A wrong `/u/<name>` sends the token to another tenant's daemon inside TLS, where it is refused and not logged |
| R13 | (Overruled by the controller.) **Tenants get the SessionStart briefing, bounded:** `curl --connect-timeout 1 --max-time 2`, plain stdout, nothing on any failure, exit 0; `capture.briefing` or `briefing=off` in `capture.conf` turns it off | The team uses only the server; without it they never get the briefing local users get | Up to 2 s at the start of a session when the server is slow or down |
| R14 | **Distillation requires `judge.provider = jev`** | The judge is what removes duplicates of explicit memories; without it, near-copies would be stored or become review items | A person without the Jev judge gets raw capture only |
| R15 | **`distill.provider` defaults to `off`** | Sending session text to a cloud model is opt-in in an open-source product; the owner turns it on in one command | New users get raw capture only until they opt in |
| R16 | **At most 40 candidates per request and `max_per_run` (8) kept**, by priority (summaries, conclusions, user statements, subagent reports). Task 12b: the kept are ranked main session before subagents, then handoff, decision, bugfix, warning, fact, howto, preference, then p; `max_per_session` (16) per session over all its runs and subagents, one slot of it kept for a handoff at the session's end (`session-end`, a compaction or the 24-hour stale end); `max_per_subagent_run` (2) subagent reports per run and half of `max_per_session` per session; a `preference_user` passage that names the project (its slug, name, folder or a path in it) stays in the project instead of `global`; a title is the first sentence, passing over `#` headings, and only when that is a table header, or 25 characters or fewer (a short heading or label), the first sentence after it that says more, at most 80 characters (§6.4, §6.5) | Bounded cost and no floods; compaction summaries and turn conclusions carry most lasting knowledge | A very long quiet-free stretch loses its lowest-priority candidates. A long session keeps memories from its first runs only: once the session's cap is full, the rest of it, compaction summaries included, is not asked about |
| R17 | **Extractive distillation with `choice` questions only**, through the existing client | Jev has no text type; the client's cache, budget, redaction and gate are reused unchanged | Passages that need the conversation are dropped (lower recall); the eval measures it |
| R18 | **A session already mined by MemPalace continues after the import's end time; the importer skips sessions SP2 holds** | MemPalace's chunks cannot be aligned with turns; a session must be held by one side | Sessions active during the cut-over may have a gap or overlap of a few minutes at the boundary |
| R19 | **Distilled memories never open review items; an explicit memory beats a distilled duplicate; distilled ones never supersede explicit ones** | The review inbox is for what people and agents wrote; machine copies must never cost a person's attention or an explicit memory | An uncertain but real update of a distilled memory stays as two linked memories instead of a question; a distilled update of an explicit memory is trashed as its duplicate even when it adds detail (Task 11b), and its passage stays in the raw capture |
| R20 | **The installer keeps `settings.json` key order as far as practical**, chosen in the plan without changing JSON output elsewhere | A person's settings file should not be reshuffled; enabling `serde_json/preserve_order` workspace-wide would change other JSON output | If not practical, keys are re-serialised (a backup exists and the output says so) |
| R21 | **Codex rollouts read tolerantly; `.zst` files skipped in v1** | The format is documented as unstable; a zstd dependency is not justified before it is seen in use | Compressed Codex sessions are not captured until a later version |
| R22 | **No request compression for tenant uploads** | It needs a new dependency; the upload runs in the background | More upstream bandwidth for large transcripts |
| R23 | **Distilled importance 2 (setting 1–3), no topics, an `(auto)` mark** | Explicit memories (default 3) rank higher and own the key-memory sections; the mark tells the agent what is machine-extracted | Some valuable distilled memories rank a little lower than their content deserves |
| R24 | **A `jev:distill` ledger row, the total still capped by `judge.daily_budget_usd`, distill capped at `budget_share`** | The owner asked for the judge's budget; a share keeps memory judging and precise recall working | Needs a consumer tag in the Jev client and ledger (small) |
| R25 | (Controller's ruling.) **`capture.raw_retention` ages out captured transcripts only**: a second pass of the `raw_retention` job over capture sources. Default `forever`; 180 days on tenants, pinned by `ALICE_CAPTURE_RAW_RETENTION` in the tenant's env file (written by `alice-server`) | The server disk is not grown, and the owner's own volume (~365k chunks a year at ~16.8 KB each with two backups) would fill it within a year; imported history must never age out by it | Transcripts older than 180 days are gone on tenants; distilled and explicit memories stay |
| R26 | (Controller's ruling.) **The owner becomes a tenant by adoption**; then the tenant setup line replaces `alice hooks install claude`, and the local hooks are removed | One memory for the owner, on the server like the team's | Sessions open during the move may finish capturing a little late (24 h stale end) |
| R27 | (The owner's decision.) **On this deployment Jev is on for every tenant with one shared key the owner manages**; the product default stays `distill.provider = off` | The owner wants the same quality for the whole team and manages one key | The owner pays for every tenant's Jev use, capped at 5 × each tenant's `judge.daily_budget_usd` ($5.00 a day at the default) |
| R28 | (Controller's ruling, 2026-10-17.) **On a server, distillation is a per-tenant switch**: `alice-server jev-key set` and rotations change only the key; `add` takes `distill.provider` from `/etc/alice/distill.default` (`off` without it); `alice-server distill on\|off <name>\|--all` and `distill status` switch and show it | A key rotation must never turn distillation on everywhere or undo an admin's per-tenant choice; distillation stays off until the flood fix (Task 12b) is accepted | The admin runs one more command per tenant to turn distillation on |

---

## 13. Questions for the controller

The controller's review (2026-10-16) settled every open point, and the owner settled the one it left open:

| Point | Outcome |
|---|---|
| Transcript samples | **Permission granted with conditions** (§11.1): at most two Claude Code transcripts and one Codex rollout, structure-only fixtures, a 16-character overlap check and a key check before committing, and copies deleted. If there is no Codex rollout on the machine, Codex stays seam-only (the reader trait and the installer's refusal path) until a sample exists |
| R15 | Accepted (product default `off`). On this deployment Jev is on for every tenant with one shared key (R27); the admin turns distillation on per tenant (R28, §9.3) |
| R19 | Accepted: machine-made memories never ask a person |
| R13 | Overruled: tenants get a bounded SessionStart briefing (§5.7) |
| Retention | A capture-scoped retention, 180 days on tenants (R25, §3.9) |
| Adoption | The owner becomes a tenant (R26, §9.4) |
| The setup line | An interface the server lane implements (§9.5) |

---

## 14. Implementation outline (for the plan)

Claude Code first, then the tenant path, then Codex, so that each step is usable:

1. **Structure-only samples** (§11.1, with the check recorded in the ledger). Then the **core reader and renderer**
   (`alice-core::capture`): the Claude Code reader, turns, the rendering contract, chunk cuts and turn keys, with the
   fixture tests.
2. **Schema and writer:** the migration, `CaptureSlice`, `CaptureStateUpdate`, and the `origin` and provenance plumbing in
   `Remember`.
3. **Capture service:** the notify endpoint, the capture worker (settle, sweep, stale end), exclusions, the
   `source.capture` event and the capture-scoped retention pass; then `alice capture status`, health and the doctor
   line.
4. **`alice hook`:** the spool variant, hook timeouts, SessionStart output, and the latency test.
5. **`alice hooks install/uninstall/status claude`.**
6. **Settings:** `[capture]`, `[distill]`, loosening, project `no_capture`.
7. **Distillation:** candidates, the request, answers to memories, the ledger consumer tag and budget share, the breaker,
   the worker, the judge's §6.6 rules, the briefing and recall marks, stats and the doctor line.
8. **The synthetic set and recorded fixtures,** the live evaluation, and `docs/benchmarks/sp2-distill.md`.
9. **The tenant path:** the transcript and briefing endpoints, `hook.sh` (with the bounded briefing), `setup.sh`, the
   public routes, the adoption test, and the script tests on both operating systems.
10. **Codex:** the rollout reader, `alice hooks install codex`, and the tenant setup's Codex part.
11. **MemPalace migration:** continue after an imported session, the importer skip, `origin = 'import'`, and backfill.
12. **Docs:** `docs/capture.md` (new: local install, removal, privacy, the cut-over), `README.md` and `README.th.md` in
    the same commit, and a "Capture for team members" section in `docs/server.md`; then the owner's real-session check,
    the gates and the final review.

Merge `feat/server-mode` in at task boundaries when it moves, and re-run the tests.
