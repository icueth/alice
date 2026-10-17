# Automatic capture

Alice can keep every Claude Code and Codex session as it happens. Each finished turn of a session is stored in the
**raw tier**, in the session's project, where `recall` finds it when you ask for transcripts (`alice recall --raw`,
`include_raw`). Optionally, ended sessions are then **distilled** into a few short memories (a decision, a fix, a
how-to, a warning, a preference, a fact or a handoff) by the Jev judge.

Capture works in two places, with the same rules:

- **Locally.** The agent's hooks run `alice hook <event>`, which only tells your daemon that a transcript grew. The daemon
  reads the file from where it stopped last time. Set it up with `alice hooks install claude` (and `codex`), or hand
  [setup-local.md](setup-local.md) to your agent.
- **On a server tenant.** A small shell script sends the new bytes of the transcript to your tenant over HTTPS. You
  install nothing but the script and a tiny Claude Code plugin, with one command. See
  [server.md, "Capture for team members"](server.md#capture-for-team-members) and [setup-server.md](setup-server.md).

This page covers what is captured, how to install and remove it locally, privacy, distillation, the settings, backfill,
moving over from MemPalace, and what `alice doctor` says when something is wrong.

## What is captured, and what never is

**The unit is a turn:** your prompt and everything the agent did until your next prompt. A turn is stored once it is
complete, which means one of these:

- the next turn has started;
- the agent stopped (`Stop`, `SubagentStop`, `PreCompact` or `SessionEnd`) and a short settle delay has passed (1.5 s
  locally, 2 s in the tenant script), because the transcript file can lag behind the event;
- locally, the file has not changed for 5 minutes; on a server tenant, every upload that is not cut at its size cap
  closes the last turn.

A last turn whose final tool call has no result yet stays open, except at a session end (or, locally, after 24 hours
without a change).

So capture lags the live session by at most one turn. A compaction summary is a turn of its own.

**Each session is one source**, named by the agent and the session id (`claude-code:session:<id>`, `codex:session:<id>`).
A Claude Code subagent's own transcript is a source of its own, linked to its session
(`claude-code:session:<id>:agent:<agent-id>`). The source never stores a file path on a server, and keeps the project it
was first captured in: a session that later `cd`s into another repository stays in the first one.

**Exactly once.** Every stored turn records a key (the id of its first record). A turn whose key is already stored, in any
source, is skipped. That covers a hook that fires twice, a daemon killed in the middle of a pass, a forked session that
copies its parent's records into a new file, a replaced or truncated file, and a tenant script that lost its place.

**How a turn is written down.** Each block becomes one line that starts with a tag. The distiller reads these tags back,
so the format is fixed:

```
[user] <your prompt>
[assistant] <the agent's text>
[tool Bash] git status --short
[result] <trimmed output>
[result error] <trimmed output>
[summary] <compaction summary>
[subagent general-purpose] <the subagent's final report>
```

| Content | What is stored |
|---|---|
| Your prompts | Kept. `<system-reminder>` spans are removed: they are injected context such as CLAUDE.md and Alice's own briefing. A slash command keeps its name and at most 300 characters of its output |
| The agent's text | Kept |
| Thinking | Dropped (`capture.include_thinking = false`) |
| Images, documents, audio | `[image]`, `[document]`, `[audio]`; the bytes are never stored |
| Read, NotebookRead | `[tool Read] <path>`, without the file's content |
| Edit, Write, MultiEdit, NotebookEdit | `[tool Edit] <path>`, without the content |
| Bash | The command (at most 300 characters) and its output, trimmed |
| Grep, Glob, LS | The pattern and the first 300 characters of the result |
| WebFetch, WebSearch | The URL or query and the first 500 characters of the result |
| A subagent call (Task, Agent) | `[subagent <type>] <description>`, then its final report, at most 2,000 characters |
| TodoWrite and similar bookkeeping | Dropped |
| Alice's and MemPalace's own memory tools | `[tool alice remember] <title, at most 120 characters>`, and the result as `[result] [omitted: memory tool]`. Memory text never comes back into the raw tier |
| Other tools | The input as compact JSON (at most 300 characters) and the output, trimmed |
| Long runs of base64-like text, or text that is mostly unprintable | `[binary removed]` |
| Progress lines, file snapshots, titles, system lines, hook output | Dropped |
| A transcript line over 4 MiB | Skipped, leaving `[record skipped: too large]` in its turn |

"Trimmed" means the head and the tail of the output, with a marker such as `[… 12,345 characters trimmed …]` between them.
The total is `capture.tool_output_chars` (default 800: the first 500 and the last 300 characters). A rendered turn is cut
into chunks of about `capture.chunk_chars` characters (default 2,000), at block boundaries where possible.

**Never stored:** the bytes of images and other attachments, the content of files the agent read or edited, the
results of memory tools, injected `system-reminder` context, and anything from a project excluded from capture (below).
Thinking is not stored unless you turn on `capture.include_thinking`. Codex rollouts compressed as `.jsonl.zst` are
skipped and counted; Alice reads only uncompressed rollouts.

**Where it shows up.** Captured chunks are found by `recall` with `include_raw` (CLI: `alice recall --raw`) and by the raw
fallback when few memories match. They never appear among the knowledge hits or in the `context` briefing.

**Volume.** A turn renders to about 1-3 KB, about one chunk. Twenty sessions a day of about 50 turns each is roughly 1,000
chunks a day.

## Local install and removal

Capture needs a running daemon, so start it at login first:

```bash
alice service install
```

### Claude Code

```bash
alice hooks install claude --dry-run    # shows what would change; writes nothing
alice hooks install claude
alice hooks status claude
```

`alice hooks install claude` edits `$CLAUDE_CONFIG_DIR/settings.json`, else `~/.claude/settings.json` (`--settings <file>`
names another file). For each of `SessionStart`, `Stop`, `SubagentStop`, `PreCompact` and `SessionEnd` it adds one hook:

```
'<path to alice>' hook <event> --agent claude-code || true
```

with a 10-second timeout as a guard. The command starts with `ALICE_HOME='<home>' ` when your data directory is not
`~/.alice`. `|| true` means a missing `alice` binary never fails a turn. <!-- TODO(confirm): the command form, the
timeouts and the output text come from the Task 8 commit 4eaa6a8, not yet merged into feat/sp2-capture. -->

- **It is careful with your file.** A missing file starts empty. A file that is not valid JSON is refused, and nothing
  changes. Other hooks and other settings stay exactly as they are, in their order. Before writing, the old file is
  copied to `settings.json.alice-backup-<time>` (mode 0600; the newest five are kept), and the new one replaces it in one
  step.
- **It is idempotent.** Running it again changes nothing. If you move the `alice` binary, run it again: it replaces the
  hooks that point at the old path.
- **It lists other memory hooks.** Hooks whose command mentions `alice` or `mempalace` (a MemPalace plugin's hooks, or an
  older SessionStart script that printed `alice context`) are listed, so you can remove them yourself. Alice's capture
  replaces them.
- **Hooks load in new sessions.** Start a new Claude Code session afterwards.

`alice hooks uninstall claude` removes exactly Alice's entries (and the lists they leave empty), with a backup.
`alice hooks status claude` says whether the hooks are installed, whether the `alice` they run exists, and when something
was last captured.

### Codex

```bash
alice hooks install codex
```

This writes `$CODEX_HOME/hooks.json`, else `~/.codex/hooks.json`, with the same five events and
`--agent codex` (Codex allows `SessionEnd` at most 3 seconds, so that hook's timeout is 3). Two Codex rules apply:

- **One format per layer.** If `~/.codex/config.toml` already holds hooks of its own (`[hooks]` tables), the install is
  refused and nothing changes. Move those hooks into `hooks.json`, or remove them, and run it again.
- **Trust.** Codex runs a hook only after you trust it. Open Codex, type `/hooks` once, and trust the Alice entries.

Codex sometimes gives no transcript path. The daemon then looks for the session's rollout file in the newest seven date
folders of `$CODEX_HOME/sessions` (the daemon's `CODEX_HOME`, so start the daemon with the same one Codex uses). Codex
gets no SessionStart briefing from the hook: the MCP instructions ask the agent to call `context` itself.

### What the hook does

`alice hook <event>` reads the hook's JSON on standard input and:

1. tells the daemon which transcript grew (`POST /api/v1/capture/notify`), with timeouts of 30 ms for finding the daemon
   and 40 ms for the request;
2. at a new Claude Code session (`source` = `startup`, `clear` or `compact`; not `resume` or `fork`, which already hold
   one), prints the project's briefing as SessionStart context, if it arrives within the hook's 90 ms. The briefing
   starts `# Context · <project>`, which the MCP instructions read as "a briefing is in the conversation";
3. when the daemon does not answer, queues the notice in the spool and starts the daemon in the background (unless
   `ALICE_NO_AUTOSTART` is set). Nothing is lost: the daemon reads the transcript from its own cursor when it runs.

It always exits 0, prints nothing else, and writes problems to `$ALICE_HOME/logs/hook.log` (one line each, never the
transcript's text; rotated to `hook.log.1` past 1 MB). The budget is 100 ms from process start to exit; the p95
reported during development is about 7 ms with the daemon up and 16-22 ms with it down ([benchmarks](benchmarks/sp2.md),
to be measured again for that page). <!-- TODO(confirm): the figures come from Task 7's report, which is not in the
repository. -->

Turn the briefing off with `alice config set capture.briefing false`.

### Checking it works

```bash
alice capture status              # the newest 50 sessions: chunks, last capture, distillation state (--json: the cursor too)
alice recall --raw "the error we fixed this morning"
alice doctor                      # the capture hooks, capture and distill lines
```

### Server tenants

On a shared server you do not run `alice hooks`. Your admin gives you one line:

```
curl -fsSL https://alice.example/u/<name>/api/v1/capture/setup.sh | sh -s -- https://alice.example/u/<name>
```

It reuses `~/.config/alice/token` and `~/.config/alice/url`, installs the hook script and the `alice-capture` plugin from
a local marketplace, writes Codex's `hooks.json` when Codex is installed, and prints how to check and remove it.

The hook script does not update itself: setup downloads it once, into `~/.config/alice/alice-capture.sh`. After a server
upgrade that changes the capture scripts, run the same setup line again to get the new one. It is idempotent, and it
reuses `~/.config/alice/token` and `~/.config/alice/url` without asking. Details:
[server.md, "Capture for team members"](server.md#capture-for-team-members).

## Privacy

Capture follows Alice's privacy rules, plus three of its own.

**Switches** (all in `config.toml`, changed live):

- `capture.enabled = false` stops all capture; the hooks still run, but nothing is stored.
- `capture.claude_code = false` or `capture.codex = false` stops one agent.
- `capture.excluded_projects = ["client-work"]` never captures those projects (a slug or any of its aliases).

**One repository:** add `no_capture = true` to its `.alice.toml`:

```toml
[privacy]
no_capture = true     # never capture agent sessions in this repository
```

Like the other keys of that file it can only tighten. A project file that is broken or cannot be read **fails closed**:
nothing of that project is captured, and its cursor waits where it is, so nothing is lost while you fix the file.

**What an exclusion does.** The turns of an excluded project are not stored, and the cursor moves past them. Their keys
are recorded (with no text), so a continued session that copies those turns into a new file does not store them either.
Turning capture back on later does **not** fill in the excluded period.

**Redaction.** Every chunk passes the redactor of its project: the global `privacy.*` rules plus the project's
`.alice.toml` (custom patterns, block mode). In `block` mode a chunk that holds a secret is replaced by a stub that
keeps the chunk's first tag and names the detectors, such as `[assistant] [withheld: github_token]`, and capture goes
on: one secret never stops a session's capture, and nothing with a secret is stored. On a server tenant, redaction is
always on.

**Cloud denial.** Captured chunks reach a remote embedding provider only for projects the cloud policy allows, as every
chunk does. Distillation never sends a denied project's text to Jev: its session waits as `skipped_cloud` and is tried
again when the policy allows it. A subagent working in a denied repository is gated by its own project, even when its
session is allowed. A subagent's final report that its parent received is part of the parent's transcript and follows the
parent's project.

**On a server tenant** the repository's `.alice.toml` is not mirrored. The capture script reads two lines of it on your
machine: `no_capture = true` (nothing of that session is sent) and `cloud_denied = true` (sent, but never distilled). A
file it cannot read skips the event. The tenant's `capture.excluded_projects` and `privacy.cloud_denied_projects` decide
the rest. Custom patterns and block mode from the repository do not apply there. See
[server.md](server.md#capture-for-team-members).

## Distillation

Distillation turns ended sessions into a few memories you would have wanted an agent to `remember`. It is **off by
default**, because it sends session text (redacted, and only for projects the cloud policy allows) to a cloud model.

### Turning it on

Distillation needs the Jev judge, because the judge is what removes duplicates of what your agents already saved:

```bash
alice config set-secret jev_api_key        # paste the key at the prompt (it is not shown)
alice config set judge.provider jev
alice config set distill.provider jev
alice doctor                                 # the distill line says `jev` and how many were kept today
```

Both `judge.provider` and `distill.provider` are protections: a change in `config.toml` takes effect only after the file
reads the same three times in a row (about 6 seconds), while `alice config set` applies it at once and says so.

**What it picks up.** Once it is on, the sessions captured in the last 7 days that still hold undistilled turns are
distilled, newest first. Older ones, and sessions captured by a backfill, are distilled only when you ask:
`alice capture distill <session-id>`. A backfilled session that you resume is distilled from its first new turn on
([Backfill](#backfill)).

### What it does

- **When.** A session is distilled when it ends (`SessionEnd`, or 24 hours without any event when it captured something
  in the last 8 days; an older one waits for you to ask), when it is compacted (`PreCompact`), when it has been quiet
  for `distill.idle_minutes` (default 15), or when you run `alice capture distill <session-id>`. A session you resume
  after it ended is open again from its first new turn: its quiet pauses do not count as ends, and its next end does.
- **Extractive.** Jev answers choice questions only, so a distilled memory is **a passage of your session**, chosen and
  classified by Jev, never new text. Alice proposes up to 40 passages per run: compaction summaries first, then each
  turn's concluding text, then statements you made, then subagent reports. Passages that hold `[withheld:`, are mostly a
  tool line, or repeat what the session already saved with `remember` are dropped first.
- **One request per run.** Jev gets one question per passage: does it hold a decision, a bugfix, a how-to, a warning, a
  preference of yours (filed under `global`, unless it names this project: its name, its folder or a path in it), a
  convention of this project, a fact, a handoff, or is it already saved, ephemeral or unclear? A passage is kept when
  its best answer is a kind of knowledge with probability at least `distill.min_confidence` (default 0.7). What is kept
  is ranked: a handoff first, then by Jev's confidence in bands (0.9 and above, 0.8 and above, below), then your session
  before its subagents within a band, then by kind (decision, bugfix, warning, fact, how-to, preference). At most
  `distill.max_per_run` (default 8) are kept per run, at most one handoff, and at most `distill.max_per_subagent_run`
  (default 2) subagent reports. A subagent's report is never the session's handoff.
- **A session keeps its best.** A session's best set, its runs and its subagents together, holds
  `distill.max_per_session` (default 16) memories for its first 100 turns and 8 more for each further 100 turns, up to
  `distill.max_per_session_cap` (default 48); subagent reports take at most half of it. Every run is asked about, a
  full session's too, so a better passage later in the session replaces the weakest one kept, once the judge has kept
  the new one (a restatement the judge trashes as a duplicate replaces nothing; one the judge relates to a kept memory,
  such as a newer value of it, replaces that memory first); the replaced memory goes to the trash (by
  `distill:cap`, and you can restore it like any other), and its passage stays in the raw capture. A handoff takes no
  slot: the session keeps only its newest one (a newer handoff supersedes the older one), and it never pushes out a
  kept memory. The cap never replaces a distilled memory that you or an agent edited, moved, restored, linked to or
  cited in a fact after it was distilled; those keep their slots. When no
  memory that may go is weaker, the new passage is not stored. A distilled memory you saved again word for word is
  yours: it leaves the set and frees its slot.
- **The memory.** The passage (at most 1,200 characters), titled with its first sentence, passing over `#` headings;
  when that is a table header, or 25 characters or fewer (a short heading or label), with the first sentence after it
  that says more (at most 80 characters). A title never holds a `[REDACTED:…]` marker (the first sentence without one is
  taken instead) and never ends inside a backtick code span. It gets a last line
  `Context: <your prompt's first sentence>` when it needs one to stand on its own. Importance `distill.importance`
  (default 2), no topics, the session as author, and provenance links to the chunks it came from. It is written through
  the normal `remember` path: redacted, deduplicated and judged.

### `(auto)`, and why it never asks you

A distilled memory is marked **`(auto)`** in the briefing and in text recall, and has `origin: "distill"` in JSON. It is
machine-extracted: rely on it as you would on a note you did not write.

- **It never asks you anything.** A distilled memory never opens a review item. A probable duplicate is put in the trash
  as a duplicate; an uncertain update of another distilled memory, or a contradiction, becomes a link between the two
  memories, without a question.
- **Your memories win.** A distilled memory never replaces one an agent or you wrote. When the judge finds that the two
  say the same thing, yours stays and the distilled one goes to the trash, whichever came first, even when the judge
  is unsure; so does a distilled memory that only updates yours. When yours came later and updates or contradicts a
  distilled memory, yours replaces it (the distilled one is superseded). None of this asks you anything. One that holds more than yours (more than twice the text, when the judge is not
  sure, or a number yours does not state) is kept and linked to yours instead. A preference of yours filed under `global` is compared with the memories of the
  session's project too, where your agent may have saved it (unless the cloud policy keeps that project at home).
- **Hidden until judged.** Recall and the briefing skip a distilled memory until the judge has looked at it, so a near
  copy of something already saved never shows twice. `get_memory` by id still returns it.
- **It stays out of the way.** Importance 2 keeps distilled memories out of the briefing's key memories, and below
  explicit memories (importance 3 by default). "Recent decisions" lists explicit decisions first. "Last handoff" is the
  newest handoff, distilled or not.

### Cost

Distillation spends from the judge's own daily budget, `judge.daily_budget_usd`, and may use at most
`distill.budget_share` of it (default 0.5), so it never starves the judge's other work. Its calls are counted on a ledger
row of their own (`jev:distill`); `alice stats` shows "distilled today: N kept, M dropped, $x", and the day's judge total
includes it. A run costs about $0.0008 for 40 passages; a typical session takes one or two runs.

- When the share is used up, sessions wait for the next UTC day (`waiting_budget`). This is not an error.
- Three failed Jev calls in a row open a breaker for 15 minutes, doubling each time it opens again, up to 6 hours. Raw
  capture never waits on any of this.

### Where each session stands

`alice capture status` shows a distillation state for each session:

| State | Meaning |
|---|---|
| `idle` | Nothing to distil now (or distillation is off) |
| `due` | Waiting for its run |
| `done` | Distilled up to its last captured turn |
| `waiting_judge` | The judge is not Jev, or no Jev key is set |
| `waiting_budget` | Today's share is used up; tried again tomorrow (UTC) |
| `skipped_cloud` | Its project is kept from the cloud; tried again when the policy allows it |
| `skipped_excluded` | Its project is excluded from capture, or the tenant script sent `no_distill` |
| `skipped_backfill` | Captured by a backfill (a subagent's row too); distilled only with `alice capture distill`, or from its first new turn once the session goes on live |
| `failed` | Five failed attempts; `alice capture distill <session-id>` tries again |

## Settings

```toml
[capture]
enabled = true              # master switch; the hooks still run, but nothing is stored when off
claude_code = true          # per agent (cursor, windsurf, opencode and gemini exist for later readers)
codex = true
excluded_projects = []      # never captured, neither raw nor distilled
include_subagents = true
include_thinking = false
tool_output_chars = 800     # 0-8,000: head 5/8, tail 3/8 of each tool output; 0 drops outputs
chunk_chars = 2000          # 500-8,000
briefing = true             # the SessionStart briefing (local hook and tenant script)
raw_retention = "forever"   # captured transcripts only; "180d" or "10GB" (at least 1d or 1MB)

[distill]
provider = "off"            # "off" | "jev"
# model                     # reserved; ignored with "jev" (judge.jev.model is used)
min_confidence = 0.7        # 0.5-0.99
max_per_run = 8             # 1-50
max_per_session = 16        # 2-200: the size of a session's best set for its first 100 turns, subagents included
max_per_session_cap = 48    # max_per_session-400: the set grows by 8 per further 100 turns, up to this
max_per_subagent_run = 2    # 0-50: subagent reports kept in one run
idle_minutes = 15           # 1-1,440
importance = 2              # 1-3
budget_share = 0.5          # 0.05-1.0 of judge.daily_budget_usd
```

Every key can be read and changed with `alice config get | set`, and set from the environment
(`capture.raw_retention` is `ALICE_CAPTURE_RAW_RETENTION`; the environment wins over `config.toml`).

**Changes that loosen a protection** take effect only after `config.toml` reads the same three times in a row, and an
`alice config set` of one is announced and named in the history:

- `capture.enabled`, or an agent's flag, turned on;
- a project removed from `capture.excluded_projects`;
- `capture.include_thinking` turned on;
- `distill.provider` set to `jev`;
- `distill.budget_share` raised;
- `capture.raw_retention` shortened;
- a repository's `no_capture` removed (through the project file's own three-look rule).

### Retention

`capture.raw_retention` ages out **captured transcripts only**: sources Alice captured, never imported MemPalace history
or other raw sources, which follow `storage.raw_retention` alone. The maintenance job `capture_retention` runs daily
(`alice maintenance run capture_retention` runs it now) and deletes in small slices. Ages count from when Alice stored a
chunk. A source whose old chunks are deleted keeps its cursor and turn keys, so its file is not captured again, and
distilled or explicit memories stay. The default is `forever` locally; a server tenant keeps 180 days
(`ALICE_CAPTURE_RAW_RETENTION=180d` in its environment file).

## Backfill

Sessions from before you installed the hooks can be captured afterwards:

```bash
alice capture backfill claude --since 2026-09-01
alice capture backfill codex
alice capture backfill claude --project my-app --distill
```

`alice capture backfill <claude|codex> [--since YYYY-MM-DD] [--project <slug>] [--distill]` finds the existing
transcripts and queues each one with the local daemon, then prints the files it queued (`--json` prints the whole
report):

- Claude Code: `<CLAUDE_CONFIG_DIR or ~/.claude>/projects/*/*.jsonl`, and each session's `subagents/` files;
- Codex: `<CODEX_HOME or ~/.codex>/sessions/**/rollout-*.jsonl`; compressed `.jsonl.zst` rollouts are skipped and
  counted;
- `--since` keeps the files modified on or after the date, counted from local midnight (an RFC 3339 time, such as
  `2026-09-01T09:00:00+07:00`, is taken as written);
- `--project <slug>` keeps the sessions whose transcript's working directory belongs to that project: the slug a hook's
  session there would get (the repository's or the folder's name). A transcript that names no working directory counts
  as `global`. Aliases and merged or renamed projects are not matched, so give the base slug.

Each session is filed under the project of the working directory its transcript records, as a hook's session would be.
The daemon reads each file as its sweep does, complete turns only: a file idle for 24 hours is read to its end and its
session marked ended, and one written to in the last 5 minutes keeps its last turn open until the sweep closes it. Live
sessions' capture goes first; the backfill is read between their passes. Turn keys and the MemPalace rule below (Codex
rollouts included) keep anything already stored or imported from coming back, so a backfill is safe to run again.

A backfill is refused while `capture.enabled`, or the agent's `capture.claude_code` / `capture.codex`, is off: the daemon
would record every turn as not captured, for good. It needs a local daemon: a server tenant cannot read files on your
machine and refuses it.

**Backfilled sessions are not distilled** unless you ask: a first backfill of months of sessions must not turn into a
burst of memories and spend. They show `skipped_backfill` (their subagents too), and the quiet trigger leaves them
alone. To distil them:

- `--distill` marks each session due once the daemon has registered it, and lists any it could not mark yet, with the
  `alice capture distill <session-id>` to run later. While distillation is off (`distill.provider = off`) it marks
  nothing: turn distillation on, then run the backfill again with `--distill`.
- `alice capture distill <session-id>` distils one session later.

**A backfilled session that you resume** becomes live again at its first new turn (`claude --resume` and `codex resume`
append to the same transcript, and the hooks fire). What is captured from then on is distilled as any session's is; its
backfilled turns, and its backfilled subagents', stay undistilled. To distil those too, run
`alice capture distill <session-id>` before you resume it. <!-- TODO(confirm): that current Claude Code and Codex
versions resume into the same file; Task 20 checks it on a real install. -->

## Moving over from MemPalace

MemPalace's Claude Code plugin mines transcripts and asks agents for checkpoint diaries. Alice's capture replaces both,
and the switch never stores a session's turns twice.

**How duplicates are avoided.** A session is held by one side. When Alice first captures a session that MemPalace already
mined (`alice import mempalace` brought it in), it stores only the turns after the import's last filing time. When the
importer meets a session Alice already captures, it skips that session's transcript drawers. Either order gives each turn
once. A Codex rollout, which MemPalace files under its file's name (`rollout-<time>-<session id>`), is matched to its
session the same way. MemPalace's checkpoint diaries stay in the raw tier, are never distilled and stop growing.

### Local cut-over, in this order

1. **Install capture:** `alice hooks install claude`. The SessionStart briefing now comes from `alice hook`, and
   transcripts are captured.
2. **Run `alice import mempalace` one last time,** so everything MemPalace caught up to now is in Alice.
3. **Remove the old SessionStart script** (for example `~/.local/bin/alice-session-start`, which printed `alice context`
   and imported MemPalace every 12 hours), with `/hooks` in Claude Code or by hand. `alice hooks install` listed it.
4. **Disable the MemPalace plugin** (`/plugin`, or `claude plugin disable <name>`). That stops its hooks and its MCP
   server. Keep `compat.mempalace` on, so prompts that still call `mempalace_*` tools keep working against Alice.
5. **Optionally trim the "MemPalace Protocol" blocks** in your CLAUDE.md files (a diary after every few tool uses).
   Capture makes them redundant, and an agent's explicit `remember` of a decision or a fix is still the better memory.
6. **Optionally turn distillation on:** `alice config set distill.provider jev` (the judge must already be `jev`).

Your MemPalace data is never deleted by any of this.

### When you move to a server tenant

If you ran capture locally and your admin moves your database to your tenant (`alice-server adopt`):

1. **Before the move,** on your machine, let capture finish: end the session in progress, or wait for the daemon's sweep
   (a few minutes). Run `alice import mempalace` one last time if MemPalace is still active.
2. **The admin adopts the database.** The capture tables travel with it.
3. **On your machine,** run `alice hooks uninstall claude` (and `codex`), and remove any old SessionStart script.
4. **Run the tenant setup line** instead of `alice hooks install`. Then disable the MemPalace plugin and trim the
   protocol blocks, as in steps 4 and 5 above.

The tenant script's first upload of a session your machine already captured starts at offset 0. The server answers with
its own cursor, the script jumps there, and any turn sent again is skipped by its key. Nothing captured locally is
stored twice. A session left open by the move is closed after 24 hours without events and distilled as usual.

## Troubleshooting

`alice doctor` has three capture lines: <!-- TODO(confirm): `capture hooks` and `capture` come from the unmerged Task 8
commit 4eaa6a8; their texts may change in its review. -->

| Line | What it says | What to do |
|---|---|---|
| `capture hooks` | `not installed` (optional) | `alice hooks install claude` (or `codex`) |
| `capture hooks` | the hooks run an `alice` that does not exist | `alice hooks install claude` (or `codex`) again, from the `alice` you use now |
| `capture` | sessions and chunks captured in the last 24 hours, and the last capture | nothing |
| `capture` | nothing was captured for 7 days, with hooks installed | `alice hooks status claude` and `alice daemon logs`; start a new session |
| `capture` | transcript lines of a shape Alice does not know were skipped | the agent's transcript format may have changed: update Alice |
| `capture` | Codex sessions gave no transcript path and no rollout was found | start the daemon with the `CODEX_HOME` Codex uses |
| `capture` | Codex rollouts are compressed (`.jsonl.zst`) | those sessions are not captured in this version |
| `distill` | `off`, or `jev, N distilled today` | nothing |
| `distill` | distillation waits for the Jev judge | `alice config set judge.provider jev` |
| `distill` | no Jev key is set | `alice config set-secret jev_api_key` |
| `distill` | Jev failed 3 times; tried again at a time | nothing: raw capture goes on, and distillation resumes by itself |
| `distill` | today's distillation share is used up | raise `distill.budget_share` or `judge.daily_budget_usd`, or wait for the next UTC day |

Other things you may see:

| What you see | Meaning | What to do |
|---|---|---|
| Nothing in `alice capture status` after a session | The hooks load in new sessions only, or the daemon was not running | Start a new session; read `$ALICE_HOME/logs/hook.log` |
| A line in `hook.log` | The hook ignored an input or could not reach the daemon (it spooled the notice) | Usually nothing; the daemon reads the transcript when it runs |
| A session's last turn is missing | The trailing turn waits for the next turn, a stop event or 5 quiet minutes | Wait; it arrives |
| `alice hooks install` refuses a file | `settings.json` (or `hooks.json`) is not valid JSON | Fix the file; nothing was changed |
| `alice hooks install codex` refuses | Codex's `config.toml` has hooks of its own | Move them into `hooks.json`, or remove them |
| Codex captures nothing | The hooks are not trusted yet | Open `/hooks` in Codex and trust the Alice entries |
| "this server cannot read files on your machine; use the capture script" | `alice hook` points at a server tenant | Use the tenant setup line instead ([server.md](server.md#capture-for-team-members)) |
| A project's sessions are not captured | It is excluded, has `no_capture = true`, or its `.alice.toml` is broken (fails closed) | Fix the repository's `.alice.toml`; capture then resumes from where it stopped |

## The HTTP routes

For people who write their own clients. Every route needs the bearer token and passes the Host and Origin checks,
except the two public scripts.

| Route | Purpose |
|---|---|
| `POST /api/v1/capture/notify` | A local hook's notice (`202`). Refused on a server tenant (`400`) |
| `POST /api/v1/capture/transcript` | A tenant upload of transcript bytes (below); its own 8 MiB body limit |
| `GET /api/v1/capture/status` | The capture switch, distillation, and the 50 newest sessions; `tenant: true` on a server |
| `POST /api/v1/capture/distill` | `{"session_id": …}` or `{"source_id": …}`: distil it now (`200 {"due": [uri…]}`, `404` when nothing is captured under it) |
| `GET /api/v1/capture/briefing?source=…&project=…` | The SessionStart briefing as `text/plain` (at most 10,000 characters); `204` when `capture.briefing` is off or `source` is `resume` or `fork` |
| `GET /api/v1/capture/setup.sh`, `GET /api/v1/capture/hook.sh` | The tenant scripts (public, static, no secret) |

### Uploading a transcript

`POST <url>/api/v1/capture/transcript?<query>`, with the file's raw bytes from `offset` as the body (at most 8,388,608
bytes; `Content-Type: application/x-ndjson`).

| Query | Required | Value |
|---|---|---|
| `agent` | yes | `claude-code` or `codex` |
| `session` | yes | The session id (`^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`) |
| `offset` | yes | The byte offset of the body's first byte in the file |
| `event` | yes | `session-start`, `stop`, `subagent-stop`, `pre-compact`, `session-end` or `sweep` |
| `subagent` | no | A subagent's id, for its own transcript |
| `project` | no | The repository's folder name, when no `X-Alice-Project` header is sent |
| `more` | no | `1`: the transcript goes on past this body (it was cut at a size cap), so its last turn stays open, and a `session-end` neither closes the turn nor ends the session |
| `no_distill` | no | `1`: store, but never distil |
| `no_capture` | no | `1`: store nothing; the cursor moves past the body |

The optional header `X-Alice-Project` (UTF-8, or percent-encoded UTF-8) names the project and wins over `project`. A local
daemon ignores both and takes the project from a `cwd` query parameter.

Answers:

- `200 {"next_offset", "turns_stored", "turns_skipped", "chunks_stored", "oversized_line"}`. Keep `next_offset`. When it
  equals the request's `offset`, nothing was taken in yet (no complete turn, or the project's rules are not known right
  now): wait until the file grows. `"oversized_line": true` means the body is one line of 8 MiB with no end: skip that
  line and go on after it.
- `409 {"next_offset": N}`: the server's cursor is elsewhere (the client lost its place). Continue from `N`.
- `400` (a bad query), `401` (the token), `413` (over 8 MiB). On `5xx`, keep the offset and try again at the next event:
  the file itself is the queue.

The tenant script sends at most 2 MiB at a time, doubles the size (up to 8 MiB) when a capped body held no complete turn,
and adds `more=1` whenever a body is as long as its cap.
