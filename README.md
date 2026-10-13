<h1 align="center">Alice</h1>

<p align="center">
  <b>Long-term memory for AI coding agents.</b><br>
  One fast, local memory shared by Claude Code, Codex, Cursor, Windsurf, OpenCode and Gemini CLI.
</p>

<p align="center">
  <i>A.L.I.C.E. — Agent Long-term Intelligent Context Engine</i>
</p>

<p align="center">
  <a href="README.th.md">ภาษาไทย</a> ·
  <a href="docs/superpowers/specs/2026-10-13-alice-core-design.md">Design spec</a> ·
  <a href="#roadmap">Roadmap</a>
</p>

> [!IMPORTANT]
> **Status: design phase.** Nothing is installable yet. This README describes the system as designed; install and usage sections show the *planned* interface and will be updated as each milestone ships. Follow the [roadmap](#roadmap) or watch the repo for releases.

---

## Why Alice

AI coding agents forget everything between sessions. Memory tools fix that, but running many agents in parallel exposes new problems: memory calls that hang while another session writes, duplicated and misfiled entries, raw transcripts burying the knowledge you actually need, and no easy way to see or fix what your agents remember.

Alice is built for that workload: many agents, many projects, one memory that stays fast, clean and correct.

## Highlights

- **Never blocks your agent.** A single daemon owns the database. Writes are acknowledged in under 50 ms; embedding, judging and imports run in the background. If the daemon is down, hooks spool to disk and nothing is lost.
- **Knowledge first, transcripts on demand.** Two tiers: short, distilled *memories* (decisions, fixes, facts, how-tos, preferences, warnings, handoffs) are searched by default; raw session transcripts are kept as evidence and searched when you ask or when knowledge runs thin.
- **Search that understands code and Thai.** Hybrid retrieval: multilingual vectors, BM25, trigram matching for languages without spaces (Thai, CJK), and identifier-aware matching (`HostLinkReconnect`, `writer_lease`, file paths). Results respect the current project, recency, importance and status.
- **Memory that corrects itself.** New memories are compared with similar existing ones: duplicates merge, updates supersede old facts, contradictions go to a review inbox. Every change is logged and can be undone.
- **Optional AI judge.** Plug in [TypeSafe Jev](https://typesafe.ai) for calibrated decisions (deduplication, routing, re-ranking) with confidence thresholds, caching and a daily budget cap. Without it, Alice uses offline heuristics.
- **Small, sharp MCP surface.** Seven core tools (`recall`, `remember`, `context`, `get_memory`, `update_memory`, `query_facts`, `assert_fact`) instead of dozens, compact token-budgeted output, built-in usage instructions, slash-command prompts and resources.
- **A briefing instead of a dump.** `context` returns what matters for the current project within a token budget: key memories, recent decisions, warnings, the last handoff and your preferences.
- **Knowledge graph with time.** Entities and relations with validity periods (`as_of` queries, timelines) and a predicate vocabulary that prevents `has_bug` / `had_bug` drift.
- **Git sync as Markdown.** Push and pull memories to any git remote as one Markdown file per memory. Readable on GitHub, opens in Obsidian, syncs across machines with record-level merging. The database file never goes into git.
- **One binary.** Daemon, CLI, MCP server and hooks in a single static Rust binary for macOS, Linux and Windows. No Python environments, no Docker.
- **Private by default.** Local-first storage, secret redaction before anything is written or sent, API keys in the OS keychain, loopback-only server with token and origin checks.
- **Graph and management UI** *(planned, SP3)*. Explore the memory graph, edit memories, resolve reviews, change every setting and run git push/pull from the browser or a desktop app.
- **Bring your MemPalace.** Import an existing MemPalace palace (drawers, diaries, knowledge graph) and keep using `mempalace_*` tool names through an optional compatibility layer.

## How it works

```
 Claude Code / Codex / Cursor / Windsurf / OpenCode / Gemini CLI
   ├─ MCP ── `alice mcp` (stdio) or http://127.0.0.1:<port>/mcp ─┐
   └─ hooks ─ `alice hook <event>` ───────────────────────────────┤
 Terminal ── `alice recall …` ────────────────────────────────────┼──► alice daemon
 Browser ─── `alice ui` ──────────────────────────────────────────┘      ├─ writer (short transactions only)
                                                                          ├─ workers: embed · judge · import · sync
                                                                          ├─ scheduler: backups · checks · cleanup
                                                                          └─ SQLite (WAL) + FTS5 + HNSW vectors
```

| Concept | Meaning |
|---|---|
| **Project** | Detected from your git repository. Name variants (`my-app`, `my_app`) resolve to one project. `global` holds user-wide preferences. |
| **Memory** | A short, self-contained piece of knowledge with a kind, topics, importance and status. |
| **Source / chunk** | Raw evidence (session transcripts, files, imports) that memories link back to. |
| **Fact** | A knowledge-graph relation between entities, valid over a period of time. |
| **Review** | A decision the system was not confident enough to make alone. |

## Installation (planned)

```bash
# Homebrew (macOS, Linux)
brew install icueth/tap/alice

# Shell installer (macOS, Linux)
curl -fsSL https://github.com/icueth/alice/releases/latest/download/alice-installer.sh | sh

# PowerShell (Windows)
irm https://github.com/icueth/alice/releases/latest/download/alice-installer.ps1 | iex

# npm (if you already have Node for your agents)
npm install -g alice-memory

# Cargo
cargo install alice-memory
```

The embedding model (~120 MB, multilingual) downloads on first use. On offline machines run `alice model fetch` beforehand.

## Quick start (planned)

```bash
alice setup                    # detect installed agents and connect them (MCP + hooks)
alice import mempalace         # optional: bring an existing MemPalace palace
alice remember --kind decision "Use SQLite WAL with a single writer thread"
alice recall "why single writer"
alice context                  # briefing for the current project
alice ui                       # open the web UI
```

## Connecting agents

`alice setup` configures everything automatically. To do it by hand, register `alice mcp` as a stdio MCP server:

<details>
<summary><b>Claude Code</b></summary>

```bash
claude mcp add --scope user alice -- alice mcp
```
</details>

<details>
<summary><b>Codex CLI</b> — <code>~/.codex/config.toml</code></summary>

```toml
[mcp_servers.alice]
command = "alice"
args = ["mcp"]
```
</details>

<details>
<summary><b>Cursor</b> — <code>~/.cursor/mcp.json</code> · <b>Windsurf</b> — <code>~/.codeium/windsurf/mcp_config.json</code> · <b>Gemini CLI</b> — <code>~/.gemini/settings.json</code></summary>

```json
{
  "mcpServers": {
    "alice": { "command": "alice", "args": ["mcp"] }
  }
}
```
</details>

<details>
<summary><b>OpenCode</b> — <code>opencode.json</code></summary>

```json
{
  "mcp": {
    "alice": { "type": "local", "command": ["alice", "mcp"] }
  }
}
```
</details>

Agents that cannot use MCP can call the CLI (`alice recall`, `alice remember`, `--json` for structured output).

## MCP tools

| Tool | Purpose |
|---|---|
| `recall` | Search memories. Filters for project, topics, kinds and dates; `max_tokens` budget; `mode: precise` re-ranks with the judge. |
| `remember` | Save a memory (`kind`, `title`, `topics`, `importance`, `supersedes`). Idempotent. |
| `context` | Token-budgeted briefing for the current project. |
| `get_memory` | Full memories with links and provenance. |
| `update_memory` | Edit, supersede, archive, forget or restore. |
| `query_facts` | Knowledge-graph lookups, `as_of` a date, or as a timeline. |
| `assert_fact` | Add, supersede or end a fact, with predicate suggestions. |

An admin toolset (`review_list`, `review_resolve`, `merge_memories`, `stats`) can be enabled in settings.

## CLI (planned)

```
alice recall <query> [--project p] [--kind k] [--raw] [--precise] [--json]
alice remember [--kind k] [--title t] [--topic t]... (<text> | -)
alice context | get <id> | facts <entity> [--as-of date] [--timeline]
alice review [list | resolve <id> <choice>]      alice undo <event-id>
alice sync [init <remote> | push | pull | status]
alice import mempalace [--palace path] [--dry-run]
alice config [get | set | list | edit | set-secret]
alice doctor | bench | ui | daemon | service | mcp
```

## Configuration

Settings live in `~/.alice/config.toml`, can be overridden per repository with `.alice.toml`, and can all be changed from the UI. Changes apply without restarting.

```toml
[embedding]
provider = "local"                  # local | ollama | openai | voyage
model = "multilingual-e5-small"

[search]
default_max_tokens = 1500

[judge]
provider = "heuristic"              # heuristic | jev
auto_apply_threshold = 0.85         # act alone above this confidence
review_threshold = 0.5              # ask a human above this confidence
daily_budget_usd = 1.0

[privacy]
redact = true
cloud_denied_projects = ["client-work"]   # never sent to cloud judges

[sync]
remote = "git@github.com:you/alice-memory.git"
auto = "idle"                       # off | idle | 15m
include_raw = false

[backup]
daily = 7
weekly = 4
```

Secrets such as the Jev API key go to the OS keychain: `alice config set-secret judge.jev.api_key`.

## Git sync

```bash
alice sync init git@github.com:you/alice-memory.git
alice sync push
alice sync pull
```

Each memory becomes `<project>/<kind>/<year>/<id>-<slug>.md` with YAML frontmatter; the knowledge graph is stored as sorted JSONL. Alice merges per record, so two machines can edit different memories without conflicts; when both edit the same memory, the newer version wins and the other goes to the review inbox. Use a **private** repository: memories can contain sensitive project details, even after redaction.

## Coming from MemPalace

Alice is inspired by [MemPalace](https://github.com/MemPalace/mempalace) and can import its palaces:

```bash
alice import mempalace --dry-run   # preview the wing → project mapping
alice import mempalace
```

| | MemPalace | Alice |
|---|---|---|
| Runtime | Python package | Single static binary |
| Concurrency | Hub with a single-writer lease | Daemon as sole DB owner; heavy work runs off the writer |
| Organisation | Wings, rooms, drawers, closets, tunnels | Projects, topics, memories, sources, links |
| Raw vs distilled | Stored together | Separate tiers; knowledge searched first |
| Self-correction | Agent calls dedicated tools (e.g. duplicate check, KG supersede) | Automatic judge-driven dedup, supersede and contradiction review |
| MCP tools | ~45 | 7 core + optional admin and compat sets |
| Sharing | Shared-brain hub | Git as Markdown with record-level merge |

Drawers mined from transcripts and files become raw sources, diaries and agent-written drawers become memories, wings map to projects (name variants merged), rooms become topics, and the knowledge graph carries over. Turn on `compat.mempalace` to keep calling `mempalace_*` tools.

## Roadmap

| Phase | Scope | Status |
|---|---|---|
| **SP1 · M1** | Store, hybrid search, daemon, MCP, CLI, HTTP API | Designing |
| **SP1 · M2** | Judge (heuristic + Jev), review inbox, undo, redaction, backups, live settings | Planned |
| **SP1 · M3** | MemPalace importer and compatibility layer | Planned |
| **SP1 · M4** | Git sync | Planned |
| **SP2** | Automatic capture and distillation, `alice setup`, hooks/plugins for every supported agent | Planned |
| **SP3** | Web UI (graph, memory manager, reviews, settings, sync, health), then a desktop app | Planned |

Design details: [Alice Core design spec](docs/superpowers/specs/2026-10-13-alice-core-design.md).

## Contributing

The project is at the design stage. Issues and discussion on the spec are welcome. Code contributions open once M1 lands; the workspace layout and conventions will be documented in `CONTRIBUTING.md`.

## License

[MIT](LICENSE). Alice is inspired by MemPalace (MIT, © MemPalace Contributors); any adapted code keeps its original notice.
