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
> **Status: M2 complete — alpha.** The store, hybrid search (Thai, English and code identifiers), the daemon, the seven MCP tools, the CLI and the HTTP API (M1) now come with a write path that looks after itself: secret redaction, an offline judge with TypeSafe Jev as an option, a review inbox, history and undo, backups and restore, live settings with a private secrets file, `alice doctor`, `alice service install` and HTTP embedding providers (M2). CI builds and tests all of it on macOS, Linux and Windows; on Windows the CLI and daemon process tests run on Unix only (see [platform support](#platform-support)). There are no packaged releases yet: [install from source](#install-from-source). Features marked *(planned)* arrive in later milestones; see the [roadmap](#roadmap). Expect breaking changes before 1.0.

---

## Why Alice

AI coding agents forget everything between sessions. Memory tools fix that, but running many agents in parallel exposes new problems: memory calls that hang while another session writes, duplicated and misfiled entries, raw transcripts burying the knowledge you actually need, and no easy way to see or fix what your agents remember.

Alice is built for that workload: many agents, many projects, one memory that stays fast, clean and correct.

## Highlights

- **Never blocks your agent.** A single daemon owns the database. Writes are acknowledged in under 50 ms; embedding and indexing run in the background. If the daemon is down or still starting, writes spool to disk and nothing is lost.
- **Knowledge first, transcripts on demand.** Two tiers: short, distilled *memories* (decisions, fixes, facts, how-tos, preferences, warnings, handoffs) are searched by default; raw session transcripts are kept as evidence and searched when you ask or when knowledge runs thin. *(Automatic transcript capture and distillation: planned, SP2.)*
- **Search that understands code and Thai.** Hybrid retrieval: multilingual vectors, BM25, trigram matching for languages without spaces (Thai, CJK), and identifier-aware matching (`HostLinkReconnect`, `writer_lease`, file paths). Results respect the current project, recency, importance and status.
- **Memory that corrects itself.** A new memory is compared with the similar ones already stored. A copy of the same text folds into the older memory; with the optional judge, updates supersede old facts and contradictions go to a review inbox (`alice review`). Every change is logged (`alice history`) and can be undone (`alice undo`).
- **Optional AI judge.** Plug in [TypeSafe Jev](https://typesafe.ai) for calibrated decisions (deduplication, kind, project routing, grading search results) with confidence thresholds, a response cache and a daily budget that is never exceeded. Without it, Alice uses offline rules that merge only text that is identical and leave everything else for you to review.
- **Small, sharp MCP surface.** Seven core tools (`recall`, `remember`, `context`, `get_memory`, `update_memory`, `query_facts`, `assert_fact`) instead of dozens, compact token-budgeted output, built-in usage instructions, slash-command prompts and resources. An admin set (reviews, merging, stats) is there when you switch it on.
- **A briefing instead of a dump.** `context` returns what matters for the current project within a token budget: key memories, recent decisions, warnings, the last handoff, your preferences and the newest diary entries (auto-save checkpoints left out). A project without importance-4 memories still gets its three best (importance 3) under key memories.
- **Knowledge graph with time.** Entities and relations with validity periods (`as_of` queries, timelines) and a predicate vocabulary that prevents `has_bug` / `had_bug` drift.
- **Private by default.** Local-first storage and a loopback-only server with token and origin checks. Secrets (API keys, tokens, passwords, private keys) in text are redacted before anything is written, queued or sent; API keys live in a private file (`secrets.toml`, mode 0600) with the OS keychain as an option; a project can be kept away from every cloud service.
- **Looks after itself.** Daily backups with rotation, integrity checks, retention and trash purge run when the daemon is idle; `alice doctor` says what is wrong and what to do next; `alice service install` starts the daemon at login. Settings change live, without a restart.
- **One binary.** Daemon, CLI and MCP server (and, in SP2, hooks) in a single Rust binary with the embedding runtime built in. No Python environments, no Docker. Other embedding providers (Ollama, OpenAI-compatible, Voyage) are a setting away.
- **Git sync as Markdown** *(planned, M4)*. Push and pull memories to any git remote as one Markdown file per memory. Readable on GitHub, opens in Obsidian, syncs across machines with record-level merging. The database file never goes into git.
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
                                                                          └─ SQLite (WAL) + FTS5 + vector indexes
```

| Concept | Meaning |
|---|---|
| **Project** | Detected from your git repository. Name variants (`my-app`, `my_app`) resolve to one project. `global` holds user-wide preferences. |
| **Memory** | A short, self-contained piece of knowledge with a kind, topics, importance and status. |
| **Source / chunk** | Raw evidence (session transcripts, files, imports) that memories link back to. |
| **Fact** | A knowledge-graph relation between entities, valid over a period of time. |
| **Review** | A decision the system was not confident enough to make alone. |
| **Event** | One recorded change (who, what, before and after). `alice undo` reverses it. |

## Install from source

You need Rust 1.99 or newer ([rustup](https://rustup.rs)) and a C/C++ compiler (Xcode Command Line Tools on macOS, `build-essential` on Debian/Ubuntu, the MSVC Build Tools on Windows).

```bash
cargo install --git https://github.com/icueth/alice alice-memory
alice model fetch      # once, before first use: the embedding model (~136 MB) into ~/.alice/models
```

Run `alice model fetch` once before first use. Without it the first daemon start downloads the model itself, and until that is done reads answer "memory daemon is starting (downloading the embedding model …)" and writes are queued; `alice daemon start` waits for the download and shows its progress. Files are pinned to one revision and checked by SHA-256; set `HF_ENDPOINT` to use a Hugging Face mirror. If the daemon cannot start (no network for the download, a damaged `alice.db`), every command says why, and `alice status` shows the last failure.

### Platform support

| Platform | Status |
|---|---|
| macOS, Apple Silicon | Supported, local embeddings |
| Linux (glibc) x86_64 and aarch64 | Supported, local embeddings |
| Windows x86_64 | Builds and links; CI runs `fmt`, `clippy` and the in-process test suite there, while the CLI and daemon process tests run on Unix only. The local model is verified on Windows by the manually started `windows-real-model` CI workflow (the real model, with the data directory under a non-ASCII path). `alice daemon start`, `status` and `stop` find a daemon that is starting or running through `daemon.pid` (its pid and start time, next to the lock: a held lock file cannot be read on Windows) and a Win32 process check; unit tests cover that on Windows CI, but no process test does. `alice service install` registers a scheduled task that starts the daemon at logon; its plan is tested, but it has not been tried on a real Windows machine. |
| macOS Intel, Linux musl | Not yet. ONNX Runtime has no prebuilt library for them, so the local model cannot be built there. The HTTP embedding providers (`ollama`, `openai`, `voyage`, see [Configuration](#configuration)) are built and tested, and `alice-embed` compiles without the local model, but the `alice` package does not yet offer a build without it and CI does not build these targets, so treat them as unsupported for now. |

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
alice model fetch              # once: the embedding model (~136 MB)
alice remember --kind decision "Use SQLite WAL with a single writer thread"   # starts the daemon if needed
alice recall "why single writer"
alice context                  # briefing for the current project
alice status                   # daemon, counts, embedding and judge backlog, open reviews, last backup
alice doctor                   # what is wrong, if anything, and what to do next
```

The project is detected from the working directory (git remote, or the directory name); memories written outside any project go to `global`. `alice import mempalace` brings an existing MemPalace in ([details](#importing-from-mempalace)). Planned: `alice setup` to connect every installed agent (SP2) and `alice ui` (SP3).

## Connecting agents

Register `alice mcp` as a stdio MCP server (`alice setup` will do this automatically in SP2). The shim starts the daemon when needed, and the client's workspace root, if it sends one, decides the project. The client's name is recorded as the author of what it writes:

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
| `recall` | Search memories. Filters for project, topics, kinds and dates; `include_raw` for transcripts; `max_tokens` budget. With a model judge configured (`judge.provider = jev`) recall is **precise** by default: the judge grades the top 20 results of every tier, transcript chunks included, and rescues answers that fall just below the vector floor (slower, about half a second). `mode: "fast"` skips the judge and `mode: "precise"` forces it; without a model judge recall is always fast. A slow or failing judge costs a recall at most `search.precise_timeout_ms` (2.5 s by default; after three failures the judge is skipped for a minute): the answer is then the fast one, with a note saying why. A recall from a repository that was never written to is judged by that repository's own `.alice.toml` and the global lists, like a remote embedding of its query. In JSON, `score` is the search's fusion score and does not follow the judge's display order; a result the judge graded also has `relevance` (its P(relevant); absent in fast mode). When the judge graded the results and none clearly answers (P(answers) below 0.5 for every one), the response says "nothing found clearly answers this; the closest matches follow" and carries `low_confidence: true` (the note comes first in the text format; a top result that holds a code or identifier of 6+ characters with a digit or `_`, or a query of 3+ words, verbatim is never flagged). A query with no letters or digits (`?`) is refused as empty. |
| `remember` | Save a memory (`kind`, `title`, `topics`, `importance`, `supersedes`). Idempotent. |
| `context` | Token-budgeted briefing for the current project. |
| `get_memory` | Full memories with links and provenance. Asking for a memory the judge folded into an older one answers with the older one. |
| `update_memory` | Edit, supersede, archive, forget or restore. |
| `query_facts` | Knowledge-graph lookups, `as_of` a date, or as a timeline. |
| `assert_fact` | Add, supersede or end a fact, with predicate suggestions. |

The server also sends usage instructions, offers `recall` and `remember-session` prompts and the `memory://context` and `memory://m/{id}` resources. An admin toolset (`review_list`, `review_resolve`, `merge_memories`, `stats`) is off by default: switch it on with `alice config set mcp.admin_tools true` (the stdio shim asks the daemon each time it lists tools, so no restart is needed).

## CLI

```
alice recall <query> [--project p] [--kind k]... [--topic t]... [--raw] [--precise | --fast] [--limit n]
             [--max-tokens n] [--since date] [--before date]
alice remember [--kind k] [--title t] [--topic t]... [--project p] [--importance 1-5]
               [--supersedes id]... (<text> | -)
alice context [--project p] [--max-tokens n]
alice get <id>... [--provenance]
alice update <id> --action edit|supersede|archive|forget|restore [--title t] [--body b] [--by id]
alice update <id> --action move --project <slug>   (CLI and HTTP only, not MCP; undoable)
alice facts <entity> [--predicate p] [--as-of date] [--timeline]
alice fact add|supersede|end <subject> <predicate> <object> [--from date]
alice history [target] [--actor a] [--limit n]       # what changed, by whom
alice undo <event-id>                                 # reverse a change (its whole group)
alice review [list [--type t] [--all] [--page n] [--limit n] | show <id> | resolve <id> <choice> | dismiss --type t [--provider p] --all [--yes]]
alice stats                    # counts, review backlog, judge costs
alice backup now | list       alice restore [backup] [--yes]
alice maintenance status | run <job>
alice config list | get <key> | set <key> <value> | unset <key> | edit
             | set-secret <name> | delete-secret <name> | test <provider>
alice projects [merge <from> <into> [--yes]] | status
alice doctor                   # checks with what to do next; exit 1 when one fails
alice daemon start | stop | status | logs | run
alice token rotate [--show]
alice service install | uninstall [--dry-run] [--target-dir d] | status
alice mcp                      # stdio MCP server for agents
alice model fetch | check      # check: load the model and embed one text (no daemon)
alice bench [--chunks n] [--memories n] [--samples n] [--real] [--dir d] [--out file.json]
alice import mempalace [--palace dir] [--dry-run] [--since date] [--map-wing wing=project]...
                       [--collection name] [--allow-remote-embedding]
alice import status
```

Every command takes `--json`. Planned: `sync` (M4), `hook` (SP2), `ui` (SP3).

## Configuration

Settings live in `~/.alice/config.toml`. Layers, later ones winning: built-in defaults, `config.toml`, a repository's `.alice.toml` (privacy keys only, below) and environment variables. Every setting can be read and changed with `alice config list | get | set | unset | edit`, and edits apply without restarting the daemon.

```toml
[judge]
provider = "jev"                    # heuristic (default) | jev
auto_apply_threshold = 0.85         # act alone at or above this probability
review_threshold = 0.5              # ask a person at or above this, below the line above
daily_budget_usd = 1.0              # a hard stop per UTC day
timeout_ms = 10000

[privacy]
redact = true
redact_mode = "redact"              # redact | block (refuse a write that contains a secret)
cloud_denied_projects = ["client-work"]   # never sent to Jev or a remote embedding provider
custom_patterns = [{ name = "ticket", regex = "TCK-[0-9]{6}" }]

[embedding]
provider = "ollama"                 # local (default) | ollama | openai | voyage
model = "nomic-embed-text"          # the provider's model name
endpoint = "http://127.0.0.1:11434/v1"   # optional; each provider has a default
# idle_unload = "10m"               # local model only: unload after this long unused ("0" = never)

[search]
default_max_tokens = 1500
default_mode = "auto"                # auto (precise with a model judge, else fast) | fast | precise
precise_timeout_ms = 2500               # the whole budget of the judge call of a precise recall

[maintenance]
enabled = true
idle_secs = 60                      # background jobs start after this much quiet
trash_days = 30                     # forgotten memories are deleted for good after this
events_days = 365                   # history older than this is pruned

[backup]
daily = 7
weekly = 4

[import]
max_upload_bytes = 4294967296       # server tenant: the largest palace archive a person may upload
```

Environment variables:

| Variable | Meaning |
|---|---|
| `ALICE_<SECTION>_<KEY>` | Any setting, the dotted key upper-cased with dots as underscores: `judge.review_threshold` is `ALICE_JUDGE_REVIEW_THRESHOLD`, `judge.jev.rate_per_sec` is `ALICE_JUDGE_JEV_RATE_PER_SEC`. The environment wins over the files. An `ALICE_*` name that is not a setting is ignored with a warning (see `alice config list` and `alice doctor`); the other settings are unaffected. |
| `ALICE_HOME` | Data directory (default `~/.alice`). |
| `ALICE_LOG` | Log filter, e.g. `info` or `alice_daemon=debug` (the daemon logs to `$ALICE_HOME/logs/daemon.log`). |
| `ALICE_AGENT` | Author name the CLI records on memories it writes (default `cli`). |
| `ALICE_NO_AUTOSTART` | When set (and not `0`), commands never start the daemon; a write while none runs is queued in `$ALICE_HOME/spool` (secrets redacted first) and applied when one starts. |
| `TYPESAFE_API_KEY`, `ALICE_EMBEDDING_API_KEY` | The Jev key and the HTTP embedding provider's key. They win over stored secrets and are never written anywhere. |
| `HF_ENDPOINT` | Hugging Face mirror for `alice model fetch`. |

**Secrets.** Keys are kept out of `config.toml`. `alice config set-secret jev_api_key` (and `embedding_api_key` for `openai` or `voyage`) reads the value from standard input, never from the command line, and stores it in `~/.alice/secrets.toml` (mode 0600). The settings API only says whether a secret is set. `secrets.backend = "keychain"` uses the OS keychain instead (on macOS it may ask for permission after each update). Two things to know: a `secrets.toml` you edit by hand is read when the judge is next rebuilt, so run `alice daemon stop` (the next command starts the daemon again) after editing it; and switching `secrets.backend` does not move stored secrets, so set them again.

**Per-repository privacy.** A repository can add `.alice.toml` at its root. It accepts only `[privacy]`, and only keys that make Alice more careful; anything else is refused:

```toml
[privacy]
block_secrets = true      # refuse writes in this project that contain a secret
cloud_denied = true       # never send this project's text to Jev or a remote embedding provider
custom_patterns = [{ name = "internal_id", regex = "ID-[0-9]{8}" }]
```

**Changes are applied carefully.** `config.toml` is checked every 2 seconds. A file or value that is invalid never replaces working settings: the previous values stay, and the error shows in `alice config list`, `/api/v1/settings` and `alice doctor`. A change that would weaken a protection (redaction off, a project removed from `cloud_denied_projects`, a higher budget, switching to Jev or to a remote embedding provider, admin tools on) is applied only when the same file is read three times in a row (about 6 seconds), so a half-saved file cannot switch redaction off for a moment; `alice config set` applies it at once and says so. Until a valid `config.toml` has loaded, nothing is sent to any cloud service. These keys need a daemon restart (`alice daemon stop`): `server.port`, `secrets.backend`, `embedding.idle_unload`, `embedding.batch_size`. Changing the embedding provider or model needs none: the new model fills in the background and takes over atomically when it is complete.

**What redaction catches.** Before a memory, a transcript chunk, a history snapshot, a queued write or a request to Jev is stored or sent, Alice replaces secrets with `[REDACTED:<type>]`: private keys, JWTs, AWS, GitHub, GitLab, Slack, Stripe, SendGrid, Vault, Anthropic and OpenAI keys, passwords in URLs, `password=`/`token=`-style assignments and long high-entropy values after a key-like name, plus your own `custom_patterns`. With `redact_mode = "block"` the write is refused instead, and the error names the field and the detector (never the secret).

**Local embedding or not.** The default is the local multilingual model. An HTTP provider (`ollama` on this machine or your network, `openai` for any OpenAI-compatible endpoint, `voyage`) receives text only for projects the cloud policy allows, and `ollama` on `localhost` counts as local. Details: [docs/benchmarks/m2-embedding.md](docs/benchmarks/m2-embedding.md).

## Judge, review and undo

When a memory is written it is compared with the memories already stored in its project, in the background, and settles as `active`:

- **Heuristic (default, offline, free).** It merges a new memory into an older one only when the text is the same after Unicode normalisation and whitespace collapse (case and punctuation count). It never merges on similarity alone: a probable duplicate, a likely update or a likely better project becomes an item in the review inbox. It sets no kinds.
- **Jev (optional).** TypeSafe's Jev model answers choice questions with probabilities. Alice asks whether the new memory duplicates, updates, contradicts or is unrelated to each of its nearest neighbours, and which kind and project fit. Above `judge.auto_apply_threshold` the decision is applied; between the two thresholds it becomes a review item; below, nothing happens. Answers are cached by request, calls are rate-limited, and the daily budget is reserved before every call, so the day's spend cannot pass `judge.daily_budget_usd`. If Jev is down, over budget or not allowed for a project, the heuristic decides and the memory is judged again later; a judge problem never fails a write. On the synthetic evaluation in [docs/benchmarks/m2-jev.md](docs/benchmarks/m2-jev.md) it cost **$0.0113 per 1,000 decisions** (a decision is one memory pair compared; input tokens at $0.042 per million), and the live smoke test of one comparison costs about **$0.00004**. That evaluation (216 pairs written for the purpose, one run) put Jev's relation accuracy at 0.917 against 0.287 for the heuristic; read the caveats on that page before relying on the numbers for real traffic.
- **Review inbox.** `alice review` lists what Alice could not settle (supersede, contradiction, merge, predicate, project, undo); `alice review show <id>` and `alice review resolve <id> <choice>` answer it. The list is paged (`--page`) and ends with the open total by type; `alice review dismiss --type merge --all` closes a whole type at once without changing any memory. The weekly merge job stops suggesting while 50 merge items wait. `context` only asks the user about review items when 10 or fewer are open.
- **History and undo.** Every change is an event: `alice history [id]` shows who changed what, `alice undo <event-id>` reverses it, together with the other changes made by the same action (a merge, a supersede). If the target changed since, Alice opens a review item instead of overwriting anything. Memory, fact, predicate and project-merge changes can be undone; a purged memory cannot come back.

## Backup and restore

```bash
alice backup now          # a checked, private copy of alice.db in ~/.alice/backups
alice backup list
alice restore             # the newest backup; or: alice restore <name or path> [--yes]
```

The daemon also makes a daily backup when it is idle, and keeps the newest of each of the last 7 days and 4 weeks (`backup.daily`, `backup.weekly`); manual backups keep their newest five; a copy taken before a database migration or a restore is kept 30 days (the newest of each kind always). Backups hold the database only, never `secrets.toml` or the token.

`alice restore` checks the backup first (a SQLite file that passes `quick_check`, with an Alice schema this build can read; a newer one is refused before anything is stopped), stops the daemon, keeps the current database as `backups/alice-before-restore-<time>.db`, puts the backup in its place with one atomic rename, and starts the daemon, which rebuilds the vector index and applies the writes agents spooled meanwhile. `alice.db` is always either the complete old database or the complete restored one: if anything fails, the old one stays. An older backup is migrated at that start (with its own pre-migration backup). `secrets.toml`, `config.toml` and the spool are never touched. It needs free space for the kept copy and the restored file, and refuses without changing anything when there is not enough.

**Memories written after the backup leave the active database.** They stay in the before-restore copy, which is kept at least 30 days (the newest one is always kept). To undo a restore, run `alice restore alice-before-restore-<time>.db`. Two more things a restore does not undo: memories you purged after the backup was taken come back with it, and text stored under older redaction rules is not scanned again.

On a large database the daemon stays stopped for the length of two copies and a check (minutes at 2.8 GB). Agents' writes are spooled at once meanwhile, and applied afterwards.

Maintenance runs on its own when the daemon has been idle for a minute and gives way as soon as a request arrives: daily backup, integrity check, vector index check, retention, trash purge and history pruning; weekly full-text index merging and merge suggestions (never diaries or status notes; one kind per group; every pair in a group is alike in vectors and in words, token overlap 0.6 with numbers and ids ignored; no memory in two open items, at most 5 per group; the shown probability is the group's lowest pairwise similarity); hourly retry of judgements that failed. `alice maintenance status` shows when each job last ran, and `alice maintenance run <job>` runs one now.

## Health, token and service

```bash
alice status                  # the daemon, counts, judge, open reviews, last backup, memory in use
alice doctor                  # every check, with what to do next; --json for scripts; exit 1 when one fails
alice token rotate [--show]   # a new API token; a running daemon switches at once
alice service install         # run the daemon at login
```

`alice doctor` works with no daemon running (and starts none). Without one it checks `config.toml` and the `ALICE_*` variables, the permissions of the data directory, the token and `secrets.toml`, the embedding model on disk, and whether a daemon or a service unit exists. With one it adds what only the daemon can see: the database (read-only after a full disk), the maintenance jobs and backups, the vector index, embeddings that failed or that the cloud policy holds back, a re-embed in progress, the judge (a missing Jev key, the day's budget), open reviews, writes waiting in the spool, settings the daemon refused or that wait for a restart, memory and free disk. Every line that is not fine says what to do next. It reports keys as set or not, never their value.

`alice token rotate` writes a new random token to `~/.alice/token` (mode 0600, replaced atomically). A running daemon switches to it at once and refuses the old one from the next request on; `alice mcp` and the CLI find the new token on their next call. With no daemon the file is replaced while the command holds the daemon lock. The token is printed only with `--show`.

`alice service install` registers a launchd agent (macOS, `~/Library/LaunchAgents/dev.alice.daemon.plist`), a systemd user unit (Linux, `~/.config/systemd/user/alice.service`) or a scheduled task at logon (Windows, named `dev.alice.daemon`; not yet tried on a real Windows machine). The unit runs `alice daemon run` with the data directory the command was run with, and holds no secret. `--dry-run` prints the files and commands and changes nothing; `--target-dir d` writes the files under `d` and runs no command. On Linux, user services stop at logout unless `loginctl enable-linger $USER` is set. `alice daemon stop` ends the daemon cleanly, so the unit does not restart it until the next login. `alice restore` stops the unit before it swaps the database and starts it again afterwards, so a unit that restarts its daemon cannot start one in the middle of a restore. `alice service uninstall` removes it.

## Git sync *(planned, M4)*

```bash
alice sync init git@github.com:you/alice-memory.git
alice sync push
alice sync pull
```

Each memory becomes `<project>/<kind>/<year>/<id>-<slug>.md` with YAML frontmatter; the knowledge graph is stored as sorted JSONL. Alice merges per record, so two machines can edit different memories without conflicts; when both edit the same memory, the newer version wins and the other goes to the review inbox. Use a **private** repository: memories can contain sensitive project details, even after redaction.

## Coming from MemPalace

Alice is inspired by [MemPalace](https://github.com/MemPalace/mempalace) and imports its palaces. The main differences:

| | MemPalace | Alice |
|---|---|---|
| Runtime | Python package | Single static binary |
| Concurrency | Hub with a single-writer lease | Daemon as sole DB owner; heavy work runs off the writer |
| Organisation | Wings, rooms, drawers, closets, tunnels | Projects, topics, memories, sources, links |
| Raw vs distilled | Stored together | Separate tiers; knowledge searched first |
| Self-correction | Agent calls dedicated tools (e.g. duplicate check, KG supersede) | Automatic judge-driven dedup, supersede and contradiction review, with undo |
| MCP tools | ~45 | 7 core + optional admin and compat sets |
| Sharing | Shared-brain hub | Git as Markdown with record-level merge |

## Importing from MemPalace

```bash
alice import mempalace --dry-run   # report what would be imported; changes nothing
alice import mempalace             # import (the daemon is started if it is not running)
alice import status                # progress of a running import, or the last report
```

- **Safe to run again.** Every drawer keeps its MemPalace id, so a re-run imports only what is new (`--since 2026-04-16` narrows it to recent drawers). A run that was stopped, or that you ended with Ctrl-C (the daemon keeps going), is resumed by running the command again.
- **MemPalace need not be stopped.** Alice copies the palace through SQLite's backup API into a private temporary folder and reads that snapshot; your palace files are opened read-only and never changed. The copy needs about the palace's size in free disk space and is removed afterwards.
- **Finding the palace.** `--palace <folder>`, else `$MEMPALACE_PALACE_PATH`, else `$MEMPAL_PALACE_PATH`, else `palace_path` in `~/.mempalace/config.json`. The `sqlite_exact` and `chroma` backends are read; `--collection <name>` picks another collection than `mempalace_drawers`.
- **Where things go:**
  - Drawers from session transcripts (`*.jsonl`, wing `sessions`) and from mined files become the raw tier: one source per transcript or file, drawers as ordered chunks. Exact repeats within one source are collapsed.
  - Diary entries and drawers written by agents (no source file, or from a curated room such as `decisions` or `runbook`) become memories, with the kind taken from the room (`decisions` is a decision, `bug-classes` a bugfix, and so on). They are stored active and are not run through the judge.
  - Wings become projects (`wing_pg_ai` and `pg_ai` are one project). Agent wings such as `wing_claude_code` become `global`, with the agent as author, unless the text names a project. `--map-wing wing_project_alice=alice` (repeatable) fixes a wing's project on a fresh import; `alice projects merge <from> <into>` fixes it afterwards (see "Fixing a wrong project" below). A re-run only adds what is new and never re-homes memories it already imported: to re-home those use `alice projects merge`, or import into a fresh home.
  - Diaries pick their project by these rules, first match wins (`--map-wing` always wins): the diary's topic equals a known project or starts with `<project>-` (unless the wing's own project already contains that name, as `alice` in `project-alice`); a project wing names its project; an agent wing whose name ends in a known project (`wing_claude_code_pgai`) names it; the folder of the transcript, then the project the text names, then `global`. `test` is never inferred for a diary. The topic and agent-wing rules only trust an established project (at least 15 drawers in its wings and transcripts, or a wing you named with `--map-wing`); the agent-wing rule skips names made only of generic agent words (`code`, `claude`, `lead`...).
  - MemPalace's auto-save diaries (topic `checkpoint`) are not memories: they are transcripts in disguise, so they are imported as raw session chunks, one source per wing and day (`mempalace-checkpoint:<wing>:<yyyy-mm-dd>`, agent `mempalace checkpoint`), in the order they were filed and with the project the diary rules above pick. They are found with `recall --raw` (and by the raw fallback when few memories match) and never appear among the knowledge hits or in the briefing. Other diaries keep importance 2.
  - MemPalace cuts a long drawer, diaries included, into fragments of 800 characters whose ids end in `_chunk_000000`, `_chunk_000001`, and so on. The import joins the fragments of one drawer back into one memory (or one chunk, for a checkpoint) before anything else: one title, the whole text, the earliest filing time, the topics of every fragment. Every fragment id still leads to it, and the report counts `fragments joined`. A drawer is joined only when none of its fragment ids is stored yet; a drawer an earlier version stored fragment by fragment is left as it is (the report warns when some fragments were missing), so to get old fragments joined, import into a fresh `ALICE_HOME`.
  - Rooms become topics; closets are dropped (Alice builds its own indexes).
  - The knowledge graph (`knowledge_graph.sqlite3` in the palace folder, else `~/.mempalace/`) becomes facts with their validity periods; similar predicate names become review items instead of being merged. Entities that have no fact are not imported. Tunnels that name two importable drawers become `relates_to` links.
- **Fixing a wrong project.** If a wing landed in the wrong project, or two projects are really one (this repository as both `alice` and `project-alice`), run `alice projects merge alice project-alice`. It moves every memory, chunk, source and fact of the first project into the second (a fact follows the memory it was learned from) and makes the first project's slug, key and aliases names of the second, so later writes from its checkout and later imports of its wing land in the project you kept. It prints what it will move and asks `[y/N]` on a terminal; scripts pass `--yes`, and without a terminal and without `--yes` it refuses. `global` cannot be merged away or merged into; `alice projects` lists the slugs. The merge is one event: `alice undo <event-id>` (printed by the command, also in `alice history`) brings the project, its names and exactly the rows that moved back. It is available on the CLI and the HTTP API (`POST /api/v1/projects/merge`), not as an MCP tool, so an agent cannot reorganise your projects.
- **Private by construction.** Secrets are redacted in everything the import writes, whatever `privacy.redact` says. Nothing leaves the machine and no judge runs; once imported, the memories are ordinary ones. Times MemPalace stored without a zone are read in this machine's UTC offset (the report prints it).
- **The report** counts every drawer: `drawers_seen` must equal the drawers imported, collapsed, skipped or already present, and a mismatch is an error and a non-zero exit. It also lists the wing to project map, items queued for review and anything skipped, with the reason.
- **Embeddings are computed in the background** with Alice's own model (MemPalace's vectors are ignored). Search works the moment the import ends, by keywords; meaning-based matching of the raw tier fills in as chunks are embedded.
- **A remote embedding provider.** If `embedding.provider` is not the local model, the import refuses to start, because every chunk of your history would be sent to that provider. `--allow-remote-embedding` says you accept that.

One 122,000-drawer palace imported in about two minutes (peak memory under 200 MB) with embedding left out, and in about four minutes with the real model, by which time the knowledge tier was embedded; the 116,000 raw chunks then embed at roughly 34 per second. Importing very large palaces while many agents are busy can slow `recall` until the import ends: see [docs/benchmarks/m3.md](docs/benchmarks/m3.md).

## MemPalace compatibility

Prompts and hooks written for MemPalace call tools named `mempalace_*`. Alice answers fifteen of them from its own data, in the shape MemPalace returns: `status`, `search`, `add_drawer`, `get_drawer`, `update_drawer`, `delete_drawer`, `diary_write`, `diary_read`, `kg_query`, `kg_add`, `kg_invalidate`, `kg_supersede`, `kg_timeline`, `list_wings` and `list_rooms`.

The layer is the setting `compat.mempalace`. A real (not `--dry-run`) import turns it on if you never set it yourself; otherwise:

```bash
alice config set compat.mempalace true     # or false to hide the tools again
```

The change applies to running clients within a second, with no restart. When the daemon cannot be reached the tools are hidden.

Register `alice mcp` under the server name your prompts expect. Claude Code names a tool `mcp__<server>__<tool>`, so prompts that call `mcp__plugin_mempalace_mempalace__mempalace_search` need the server entry to be called `plugin_mempalace_mempalace`:

```bash
claude mcp add --scope user plugin_mempalace_mempalace -- alice mcp
```

For other clients give the same name to the entry in their configuration (`mcp_servers.plugin_mempalace_mempalace` in Codex, and so on). Disable the real MemPalace plugin first, or two servers answer to one name. One entry is enough: it serves the seven Alice tools and, while the layer is on, the fifteen `mempalace_*` tools.

Differences to know about:

- `similarity` in search results is relative: 1.0 for the best hit, the others scaled to it. Alice's scores are not cosine distances, so a prompt that thresholds on MemPalace distances will behave differently.
- Writes go through the normal Alice paths: redacted, judged, recorded in `alice history` and undoable. A `wing` that is not an Alice project files the memory into the current project (the answer says so); an agent never creates a project this way.
- Drawer ids from your palace keep working (`mempalace_get_drawer`, `update_drawer`, `delete_drawer`), including ids of drawers that were collapsed into an earlier identical one.
- The other MemPalace tools (`mine`, `traverse`, tunnels, `check_duplicate` and so on) are not listed, and calling one returns a message naming the Alice equivalent instead of an error.

## Performance

Measured for M2 on an Apple M4 Pro with `alice bench` ([full results](docs/benchmarks/m2.md)), one run each, p95:

| | Budget | 100k chunks, local model | 1M chunks, local model |
|---|---|---|---|
| `remember` acknowledgement | < 50 ms | 4.4 ms | 13.4 ms |
| `recall` (knowledge) | < 200 ms | 6.2 ms | 15.6 ms |
| `recall` with `include_raw` | < 200 ms | 26 ms | 119 ms |
| `context` | < 150 ms | 0.43 ms | 0.50 ms |
| `alice mcp` start-up (spawn to `initialize` reply) | < 20 ms | 4.8 ms (p50), 5.2 ms (p95) | |
| Daemon memory: peak RSS | < 800 MB RSS at 1M chunks | 534 MB | **564 MB, met** |
| Physical footprint, peak | | 511 MB | 585 MB |

M1 missed the memory budget (about 1.4 GB projected with the model). M2 meets it: chunk vectors moved into a 53 MB sign-bit index (the int8 graph took 508 MB) and the embedding model runs on ONNX Runtime directly (about 350 MB loaded instead of 600). The 1M run fills the index with synthetic vectors under the real model's id (embedding a million chunks takes hours; a vector costs the same whatever its values), then starts a real daemon that loads the real model, embeds 256 new memories and serves four bursts of 64 concurrent recalls (no errors). Search quality is unchanged: the gold set keeps MRR 1.000 with all 13 queries in the top 3, and recall@10 of the chunk index against the exact top 10 is 0.99 at 96,000 real passages.

## Use Alice from a server

A small team can share one Linux server. Each person gets their own Alice daemon behind `https://<host>/u/<name>`, and agents (Claude Code, Codex and others) connect straight to the MCP endpoint `https://<host>/u/<name>/mcp` with a bearer token. No client binary is needed, so this also works on Windows. An optional shared team memory lets a person tell their agent "save this to the shared memory"; every team member can read it, and only its author or the admin can change it. The server's admin can read every person's memory, and a repository's `.alice.toml` does not apply over direct MCP. The server needs an x86-64 CPU with AVX (AVX2 is not needed: `deploy/server/build-release` builds the server binary with Microsoft's ONNX Runtime).

The admin guide (setup, `alice-server`, disk budget, backups, privacy) and the setup for each agent are in [docs/server.md](docs/server.md).

## Roadmap

| Phase | Scope | Status |
|---|---|---|
| **SP1 · M1** | Store, hybrid search, daemon, MCP, CLI, HTTP API | Done (alpha) |
| **SP1 · M2** | Judge (heuristic + Jev), review inbox, undo, redaction, backups and restore, maintenance, live settings, doctor, service install, HTTP embedding providers, Windows build, the 800 MB memory budget | Done (alpha) |
| **SP1 · M3** | MemPalace importer and compatibility layer | Built, in final review (alpha) |
| **SP1 · M4** | Git sync | Planned |
| **SP2** | Automatic capture and distillation, `alice setup`, hooks/plugins for every supported agent | Planned |
| **SP3** | Web UI (graph, memory manager, reviews, settings, sync, health), then a desktop app | Planned |

Design details: [Alice Core design spec](docs/superpowers/specs/2026-10-13-alice-core-design.md).

## Contributing

Issues, discussion of the spec and pull requests are welcome. The workspace has four crates: `alice-core` (model, storage, search, judge), `alice-embed` (embedding providers), `alice-daemon` (writer, workers, HTTP API, MCP server, client) and `alice` (the binary, package `alice-memory`). Before opening a pull request run:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## License

[MIT](LICENSE). Alice is inspired by MemPalace (MIT, © MemPalace Contributors); any adapted code keeps its original notice.
