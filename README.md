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
> **Status: M1 complete — alpha.** The store, hybrid search (Thai, English and code identifiers), the daemon, the seven MCP tools, the CLI and the HTTP API work, and CI tests them on macOS, Linux and Windows. There are no packaged releases yet: [install from source](#install-from-source). Features marked *(planned)* arrive in later milestones; see the [roadmap](#roadmap). Expect breaking changes before 1.0.

---

## Why Alice

AI coding agents forget everything between sessions. Memory tools fix that, but running many agents in parallel exposes new problems: memory calls that hang while another session writes, duplicated and misfiled entries, raw transcripts burying the knowledge you actually need, and no easy way to see or fix what your agents remember.

Alice is built for that workload: many agents, many projects, one memory that stays fast, clean and correct.

## Highlights

- **Never blocks your agent.** A single daemon owns the database. Writes are acknowledged in under 50 ms; embedding and indexing run in the background. If the daemon is down or still starting, writes spool to disk and nothing is lost.
- **Knowledge first, transcripts on demand.** Two tiers: short, distilled *memories* (decisions, fixes, facts, how-tos, preferences, warnings, handoffs) are searched by default; raw session transcripts are kept as evidence and searched when you ask or when knowledge runs thin. *(Automatic transcript capture and distillation: planned, SP2.)*
- **Search that understands code and Thai.** Hybrid retrieval: multilingual vectors, BM25, trigram matching for languages without spaces (Thai, CJK), and identifier-aware matching (`HostLinkReconnect`, `writer_lease`, file paths). Results respect the current project, recency, importance and status.
- **Memory that corrects itself** *(planned, M2)*. New memories are compared with similar existing ones: duplicates merge, updates supersede old facts, contradictions go to a review inbox. Every change is logged and can be undone.
- **Optional AI judge** *(planned, M2)*. Plug in [TypeSafe Jev](https://typesafe.ai) for calibrated decisions (deduplication, routing, re-ranking) with confidence thresholds, caching and a daily budget cap. Without it, Alice uses offline heuristics.
- **Small, sharp MCP surface.** Seven core tools (`recall`, `remember`, `context`, `get_memory`, `update_memory`, `query_facts`, `assert_fact`) instead of dozens, compact token-budgeted output, built-in usage instructions, slash-command prompts and resources.
- **A briefing instead of a dump.** `context` returns what matters for the current project within a token budget: key memories, recent decisions, warnings, the last handoff and your preferences.
- **Knowledge graph with time.** Entities and relations with validity periods (`as_of` queries, timelines) and a predicate vocabulary that prevents `has_bug` / `had_bug` drift.
- **Git sync as Markdown** *(planned, M4)*. Push and pull memories to any git remote as one Markdown file per memory. Readable on GitHub, opens in Obsidian, syncs across machines with record-level merging. The database file never goes into git.
- **One binary.** Daemon, CLI and MCP server (and, in SP2, hooks) in a single Rust binary with the embedding runtime built in. No Python environments, no Docker.
- **Private by default.** Local-first storage and a loopback-only server with token and origin checks. *(Planned, M2: secret redaction before anything is written or sent; API keys in a private file, with the OS keychain as an option.)*
- **Graph and management UI** *(planned, SP3)*. Explore the memory graph, edit memories, resolve reviews, change every setting and run git push/pull from the browser or a desktop app.
- **Bring your MemPalace** *(planned, M3)*. Import an existing MemPalace palace (drawers, diaries, knowledge graph) and keep using `mempalace_*` tool names through an optional compatibility layer.

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

## Install from source

You need Rust 1.99 or newer ([rustup](https://rustup.rs)) and a C/C++ compiler (Xcode Command Line Tools on macOS, `build-essential` on Debian/Ubuntu, the MSVC Build Tools on Windows).

```bash
cargo install --git https://github.com/icueth/alice alice-memory
alice model fetch      # the embedding model (~136 MB) into ~/.alice/models
```

`alice model fetch` is optional on a connected machine (the daemon downloads the model on first start), but doing it up front keeps the first `remember` fast and works ahead of going offline. Files are pinned to one revision and checked by SHA-256; set `HF_ENDPOINT` to use a Hugging Face mirror.

### Platform support

| Platform | Status |
|---|---|
| macOS, Apple Silicon | Supported, local embeddings |
| Linux (glibc) x86_64 and aarch64 | Supported, local embeddings |
| Windows x86_64 | Builds and passes CI; not yet tested in daily use |
| macOS Intel, Linux musl | Not yet: ONNX Runtime has no prebuilt library for them, so local embeddings are unavailable. They will work with an HTTP embedding provider (planned, M2). |

### Packaged installers (planned)

```bash
# Homebrew (macOS, Linux)
brew install icueth/tap/alice

# Shell installer (macOS, Linux)
curl -fsSL https://github.com/icueth/alice/releases/latest/download/alice-installer.sh | sh

# PowerShell (Windows)
irm https://github.com/icueth/alice/releases/latest/download/alice-installer.ps1 | iex

# npm (if you already have Node for your agents)
npm install -g alice-memory

# Cargo (from crates.io)
cargo install alice-memory
```

## Quick start

```bash
alice remember --kind decision "Use SQLite WAL with a single writer thread"   # starts the daemon if needed
alice recall "why single writer"
alice context                  # briefing for the current project
alice status                   # daemon, counts, embedding backlog
```

The project is detected from the working directory (git remote, or the directory name); memories written outside any project go to `global`. Planned: `alice setup` to connect every installed agent (SP2), `alice import mempalace` (M3) and `alice ui` (SP3).

## Connecting agents

Register `alice mcp` as a stdio MCP server (`alice setup` will do this automatically in SP2). The shim starts the daemon when needed, and the client's workspace root, if it sends one, decides the project:

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
| `recall` | Search memories. Filters for project, topics, kinds and dates; `include_raw` for transcripts; `max_tokens` budget. *(Planned, M2: `mode: precise` re-ranks with the judge.)* |
| `remember` | Save a memory (`kind`, `title`, `topics`, `importance`, `supersedes`). Idempotent. |
| `context` | Token-budgeted briefing for the current project. |
| `get_memory` | Full memories with links and provenance. |
| `update_memory` | Edit, supersede, archive, forget or restore. |
| `query_facts` | Knowledge-graph lookups, `as_of` a date, or as a timeline. |
| `assert_fact` | Add, supersede or end a fact, with predicate suggestions. |

The server also sends usage instructions, offers `recall` and `remember-session` prompts and the `memory://context` and `memory://m/{id}` resources. An admin toolset (`review_list`, `review_resolve`, `merge_memories`, `stats`) is planned for M2.

## CLI

```
alice recall <query> [--project p] [--kind k]... [--topic t]... [--raw] [--limit n]
             [--max-tokens n] [--since date] [--before date]
alice remember [--kind k] [--title t] [--topic t]... [--project p] [--importance 1-5]
               [--supersedes id]... (<text> | -)
alice context [--project p] [--max-tokens n]
alice get <id>... [--provenance]
alice update <id> --action edit|supersede|archive|forget|restore [--title t] [--body b] [--by id]
alice facts <entity> [--predicate p] [--as-of date] [--timeline]
alice fact add|supersede|end <subject> <predicate> <object> [--from date]
alice projects | status
alice daemon start | stop | status | logs | run
alice mcp                      # stdio MCP server for agents
alice model fetch
alice bench [--chunks n] [--memories n] [--real] [--out file.json]
```

Every command takes `--json`. Planned: `review`, `undo` (M2), `config`, `doctor`, `service`, `token rotate` (M2), `import mempalace` (M3), `sync` (M4), `hook` (SP2), `ui` (SP3).

## Configuration

M1 has no settings file yet. It reads these environment variables:

| Variable | Meaning |
|---|---|
| `ALICE_HOME` | Data directory (default `~/.alice`). |
| `ALICE_LOG` | Log filter, e.g. `info` or `alice_daemon=debug` (the daemon logs to `$ALICE_HOME/logs/daemon.log`). |
| `ALICE_AGENT` | Author name the CLI records on memories it writes (default `cli`). |
| `HF_ENDPOINT` | Hugging Face mirror for `alice model fetch`. |

*(Planned, M2)* Settings will live in `~/.alice/config.toml`, can be overridden per repository with `.alice.toml`, and can all be changed from the UI. Changes apply without restarting.

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

Secrets such as the Jev API key will be stored in `~/.alice/secrets.toml` (mode 0600) with `alice config set-secret judge.jev.api_key`; the OS keychain will be an opt-in alternative.

## Git sync *(planned, M4)*

```bash
alice sync init git@github.com:you/alice-memory.git
alice sync push
alice sync pull
```

Each memory becomes `<project>/<kind>/<year>/<id>-<slug>.md` with YAML frontmatter; the knowledge graph is stored as sorted JSONL. Alice merges per record, so two machines can edit different memories without conflicts; when both edit the same memory, the newer version wins and the other goes to the review inbox. Use a **private** repository: memories can contain sensitive project details, even after redaction.

## Coming from MemPalace *(importer planned, M3)*

Alice is inspired by [MemPalace](https://github.com/MemPalace/mempalace) and will import its palaces:

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

## Performance

Measured for M1 on an Apple M4 Pro with `alice bench` ([full results](docs/benchmarks/m1.md)), p95:

| | Budget | 100k chunks, local model | 1M chunks |
|---|---|---|---|
| `remember` acknowledgement | < 50 ms | 4.3 ms | 2.8 ms |
| `recall` (knowledge) | < 200 ms | 9.8 ms | 10.5 ms |
| `recall` with `include_raw` | < 200 ms | 47 ms | 172 ms |
| `context` | < 150 ms | 0.3 ms | 0.5 ms |
| Daemon memory | < 800 MB at 1M chunks | ~0.7 GB | ~0.7 GB without the model; ~1.3 GB projected with it (not yet met) |

The 1M-chunk run uses a fake embedder; one loaded copy of the local model takes ~600 MB on its own.

## Roadmap

| Phase | Scope | Status |
|---|---|---|
| **SP1 · M1** | Store, hybrid search, daemon, MCP, CLI, HTTP API | Done (alpha) |
| **SP1 · M2** | Judge (heuristic + Jev), review inbox, undo, redaction, backups, live settings, HTTP embedding providers | Next |
| **SP1 · M3** | MemPalace importer and compatibility layer | Planned |
| **SP1 · M4** | Git sync | Planned |
| **SP2** | Automatic capture and distillation, `alice setup`, hooks/plugins for every supported agent | Planned |
| **SP3** | Web UI (graph, memory manager, reviews, settings, sync, health), then a desktop app | Planned |

Design details: [Alice Core design spec](docs/superpowers/specs/2026-10-13-alice-core-design.md).

## Contributing

Issues, discussion of the spec and pull requests are welcome. The workspace has four crates: `alice-core` (model, storage, search), `alice-embed` (embedding providers), `alice-daemon` (writer, workers, HTTP API, MCP server, client) and `alice` (the binary, package `alice-memory`). Before opening a pull request run:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## License

[MIT](LICENSE). Alice is inspired by MemPalace (MIT, © MemPalace Contributors); any adapted code keeps its original notice.
