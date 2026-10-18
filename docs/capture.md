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

A last turn whose final tool call has no result yet stays open until the session ends. A session that never says so (it
crashed, was killed, or its app was closed in the middle of a turn) is closed as if it had: once its transcript has not
changed for **6 hours**, its last turn is stored, whatever it waits for, and the session ends. Locally the daemon's
sweep does this, also for a session resumed after its end; on a server tenant the capture script does it at the first
session start after those 6 hours (it sweeps only then). A session that goes on afterwards is captured as usual: its
new turns are stored, a tool result that came late is stored as the rest of the closed turn, and nothing is stored
twice. Locally, a new session's start waits up to 24 hours for its transcript (Claude Code writes it with the first
message), so a session killed during its very first turn is closed the same way.

So capture lags the live session by at most one turn. A compaction summary is a turn of its own.

**Each session is one source**, named by the agent and the session id (`claude-code:session:<id>`, `codex:session:<id>`).
A Claude Code subagent's own transcript is a source of its own, linked to its session
(`claude-code:session:<id>:agent:<agent-id>`). The source never stores a file path on a server, and keeps the project it
was first captured in: a session that later `cd`s into another repository stays in the first one.

**A subagent killed in its first run** (an app restart, a limit hit) never sends `SubagentStop`. Its session's events
find it instead: Alice looks for the subagent transcripts Claude Code wrote beside the session's
(`<session>/subagents/agent-<id>.jsonl`) that it has no source for, at most five per event, so a session with more is
finished over later events. Locally that happens at each pass of the session and at the daemon's sweep; on a server
tenant at the session's `Stop`, `PreCompact`, `SessionEnd` and `SessionStart`, and at every session start for every
captured session. A subagent found this way follows the same rules as any source, the 6-hour close included, and a
`SubagentStop` that comes later goes on from where it stopped. A Codex sub-agent writes a rollout of its own; its
`SubagentStop` names it, and that rollout is its own source (`codex:session:<id>:agent:<agent-id>`). A Codex sub-agent
killed before its `SubagentStop` is not found yet: its rollout lies in the date folders under its own thread id.

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
[agent /root/review_w1 → /root] <a Codex agent's message to another>
```

| Content | What is stored |
|---|---|
| Your prompts | Kept. `<system-reminder>` spans are removed: they are injected context such as CLAUDE.md and Alice's own briefing. A slash command keeps its name and at most 300 characters of its output |
| The agent's text | Kept |
| Thinking | Dropped (`capture.include_thinking = false`) |
| Images, documents, audio | `[image]`, `[document]`, `[audio]`; the bytes are never stored |
| Read, NotebookRead | `[tool Read] <path>`, without the file's content |
| Edit, Write, MultiEdit, NotebookEdit | `[tool Edit] <path>`, without the content |
| Bash | The command on one line (at most 300 characters) and its output, trimmed. A secret written over several lines, such as a key's body in a heredoc, keeps its line breaks |
| Grep, Glob, LS | The pattern and the first 300 characters of the result |
| WebFetch, WebSearch | The URL or query and the first 500 characters of the result |
| A subagent call (Task, Agent) | `[subagent <type>] <description>`, then its final report, at most 2,000 characters |
| A message between agents of a Codex multi-agent session | `[agent <author> → <recipient>] <text>`, with the agents' paths (`/root`, `/root/<name>`); an opaque `encrypted_content` part is never stored |
| TodoWrite and similar bookkeeping | Dropped |
| Alice's and MemPalace's own memory tools | `[tool alice remember] <title, at most 120 characters>`, and the result as `[result] [omitted: memory tool]`. Memory text never comes back into the raw tier |
| Other tools | The input as compact JSON (at most 300 characters) and the output, trimmed |
| Long runs of base64-like text, or text that is mostly unprintable | `[binary removed]` |
| Progress lines, file snapshots, titles, system lines, hook output | Dropped |
| Bookkeeping lines of a type Alice does not know yet, holding none of `message`, `content`, `text`, `summary`, `arguments`, `input`, `output`, `prompt`, `command`, `result`, `error`, `thinking`, `reasoning` at any depth | Dropped and counted apart (agents add such lines with new releases). A line of an unknown type that holds one of those keys anywhere is counted as unrecognised, and `alice doctor` warns |
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

**Volume.** A turn with tool calls renders to many chunks: measured on the owner's real sessions, about 19 chunks per
turn (801 for 43 turns, 3,403 for 182), 83 % of them from subagents, about 2,100 chunks a day of session time
([benchmarks/sp2.md](benchmarks/sp2.md)).

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

with a 10-second timeout as a guard. The path is the full path of the `alice` you ran the install with (symlinks
resolved), in single quotes. The command starts with `ALICE_HOME='<home>' ` when your data directory is not `~/.alice`. `|| true` means a
missing `alice` binary never fails a turn. The install prints the five commands it added and ends with
`Hooks load in new sessions. To remove them: alice hooks uninstall claude`.

- **It is careful with your file.** A missing file starts empty. A file that is not valid JSON is refused, and nothing
  changes. Other hooks and other settings stay exactly as they are, in their order. A file that would not change is not
  written. Otherwise the old file is copied to `settings.json.alice-backup-<time>` (mode 0600; the newest five are
  kept), and the new one replaces it in one step. If the file changed while Alice was editing it, nothing is written
  and you run the command again.
- **It is idempotent.** Running it again changes nothing and says so (`Alice's capture hooks are already in …; nothing
  changed.`). If you move the `alice` binary, run it again: it replaces the hooks that point at the old path.
- **It lists other memory hooks.** Hook commands that run something named `alice…` or `mempalace…` (a MemPalace plugin's
  hooks, or an older SessionStart script such as `alice-session-start` that printed `alice context`) are listed under
  "Other hooks mention alice or mempalace", so you can remove them yourself. Alice's capture replaces them. Each is named
  by its event and that program's name (`SessionStart: alice-session-start …`), never by its whole command, which may
  hold a key.
- **Hooks load in new sessions.** Start a new Claude Code session afterwards.

`alice hooks uninstall claude` removes exactly Alice's entries (and the groups and lists they leave empty), with a
backup. `alice hooks status claude` prints the file and `installed`, `not installed`, or `on N of the 5 events only`;
then each `alice` the hooks run (with `which does not exist` when it is gone), and the last capture (`unknown` while the
daemon is not running; `status` never starts it). `--json` works for all three commands; it never prints the file.

### Codex

```bash
alice hooks install codex
```

This writes `$CODEX_HOME/hooks.json`, else `~/.codex/hooks.json`, with the same five events and
`--agent codex` (Codex allows `SessionEnd` at most 3 seconds, so that hook's timeout is 3). Two Codex rules apply:

- **One format per layer.** If `$CODEX_HOME/config.toml` (else `~/.codex/config.toml`) already holds hooks of its own
  (`[hooks]` tables or `[[hooks.<Event>]]` arrays, such as OpenRig's), the five hooks go into that file instead, as
  `[[hooks.<Event>]]` tables in a marked block at its end:

  ```toml
  # >>> alice capture (Codex) >>>
  # Alice capture hooks; `alice hooks uninstall codex` removes this block.
  [[hooks.SessionStart]]
  [[hooks.SessionStart.hooks]]
  type = "command"
  command = "'/path/to/alice' hook session-start --agent codex || true"
  timeout = 10
  …
  # <<< alice capture (Codex) <<<
  ```

  The old file is saved as `config.toml.bak-alice` first. No other line changes, and the new file is written only if
  it reads back as TOML that means what the old one meant plus Alice's five hooks; otherwise the install is refused
  and nothing changes (for example when `hooks.<Event>` is a list written inline, which no table can extend). A block
  that is already Alice's is left byte for byte, so Codex keeps trusting it; one that differs (a moved `alice`) is
  replaced where it is. Tables Codex adds inside the block (its `[hooks.state."…"]` trust records) are kept. While
  `hooks.json` already holds Alice's hooks, they stay there. `alice hooks uninstall codex` removes exactly the block
  (and Alice's entries in `hooks.json`); `--settings <file>` still refuses while `config.toml` holds hooks.
- **Trust.** Codex runs a new hook only after you trust it, and Alice never does that for you. The install's last line
  says so: the next time you open Codex it asks "Hooks need review"; choose **Trust all and continue** (or review the
  Alice entries with `/hooks`).

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
transcript's text; rotated to `hook.log.1` past 1 MB). The budget is 100 ms from process start to exit; the measured
p95 is under 10 ms with the daemon up, about 15 ms with it down, and about 22 ms when the hook also starts the daemon
([benchmarks](benchmarks/sp2.md); release build, macOS).

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

The tenant hook script updates itself from your Alice server once a day, over https, and updates the plugin's
`hooks.json` when that changes. Re-run the setup line only to repair an install, or once if `status` shows no
`hook script:` line (a script from before self-updates).

The check runs in the hook's background worker (never in the hook itself), at the first event a day or more after the
last check and before that event's upload. It writes one line to `~/.config/alice/capture.log` when something happens:

- an update: `capture hook updated <old> → <new>`, where a version is the first 12 hex digits of the SHA-256 of the
  hook script as built into the server, before its version line is filled in;
- a refusal, which keeps the installed script byte for byte: `capture hook update refused (HTTP 200): <why>; the
  installed script stays`, where `<why>` is a check the download failed (it is empty, cut short, fails `sh -n`, or fails
  a trial run, for example), or `capture hook update refused (HTTP <code>, not 200); …` for another 2xx or 3xx answer;
- a server it could not reach, or one that answered with an HTTP error (400 or higher): `capture hook update check
  failed (curl exit <n>, HTTP <code>); the installed script stays`. Either way it asks again a day later.

A tenant address on plain `http://` (only `127.0.0.1` or `localhost` is accepted, for tests) never updates itself: the
log says `capture hook update skipped: it runs over https:// only, …`. Details:
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
no_capture = true     # never capture agent sessions started in this repository
```

Like the other keys of that file it can only tighten. A project file that is broken or cannot be read **fails closed**:
nothing of that project is captured, and its cursor waits where it is, so nothing is lost while you fix the file.

**What an exclusion does.** The turns of an excluded project are not stored, and the cursor moves past them. Their keys
are recorded (with no text), so a continued session that copies those turns into a new file does not store them either.
Turning capture back on later does **not** fill in the excluded period.

**Redaction.** Every chunk passes the redactor of its project: the global `privacy.*` rules plus the project's
`.alice.toml` (custom patterns, block mode). In `block` mode a chunk that holds a secret is replaced by a stub that
keeps the chunk's first tag and names the detectors, such as `[assistant] [withheld: github_token]`, and capture goes
on: one secret never stops a session's capture, and nothing with a secret is stored. With `privacy.redact = false` (on
a server tenant, the admin's `alice-server secrets keep`) captured secrets are stored as written; distillation still
redacts every chunk it reads before it cuts anything from it, so Jev never sees a secret and a distilled memory never
holds one. A chunk is never cut inside a token-like run (a key, a hash, base64): a token too long for a chunk is
replaced by the trim marker.

**Cloud denial.** Captured chunks reach a remote embedding provider only for projects the cloud policy allows, as every
chunk does. Distillation never sends a denied project's text to Jev: its session waits as `skipped_cloud`, and the
policy is asked again every 5 minutes ([the gates](#the-gates)). A subagent working in a denied repository is gated by
its own project, even when its session is allowed. A subagent's final report that its parent received is part of the
parent's transcript and follows the parent's project.

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

### The gates

Before each run the distiller checks, in this order:

1. **The switches.** `distill.provider = off`, or capture is off (`capture.enabled`, or the agent's own flag): the session
   stays `idle` and is distilled once they are on again.
2. **The judge.** `judge.provider` is not `jev`, no Jev key is set, or the Jev judge is still starting: `waiting_judge`.
   Distillation needs the Jev judge because the judge is what removes doubles.
3. **The project's capture rules.** The project is excluded (`capture.excluded_projects`, or its `.alice.toml` says
   `no_capture = true`), or the tenant script sent `no_distill`: `skipped_excluded`. While the project's rules cannot be
   known (a broken or unreadable `.alice.toml`, no valid `config.toml` loaded since the daemon started, or a loosening
   of either still waiting for its three looks), the session is due again in 5 minutes.
4. **The cloud policy.** The project, under any of its names, is in `privacy.cloud_denied_projects`, or its
   `.alice.toml` says `cloud_denied = true`: `skipped_cloud`, asked again every 5 minutes.
5. **The breaker.** Three Jev requests in a row failed: the session is due again when the breaker closes.

Each subagent transcript is checked against its own project's rules (3 and 4): a subagent that is not allowed keeps its
chunks and shows why in its own state, while the rest of the session is distilled. The Jev client checks the cloud
policy again, for every project a request covers, and redacts the text with each project's rules before it is sent.

### What it does

- **When.** A session is distilled when it ends (`SessionEnd`, the close of a session left in the middle of a turn
  after 6 hours, or 24 hours without any event when it captured something in the last 8 days; an older one waits for
  you to ask), when it is compacted (`PreCompact`), when it has been quiet
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
  cited in a fact after it was distilled; those keep their slots. When no memory that may go is weaker, the new passage
  is not stored. A distilled memory you saved again word for word is yours: it leaves the set and frees its slot.
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
  is unsure; so does a distilled memory that only updates yours. A distilled one that holds more than yours (more than
  twice the text, when the judge is not sure, or a number yours does not state) is kept and linked to yours instead.
  When yours came later and updates or contradicts a distilled memory, yours replaces it (the distilled one is
  superseded). None of this asks you anything. A preference of yours filed under `global` is compared with the
  memories of the session's project too, where your agent may have saved it (unless the cloud policy keeps that
  project at home).
- **Hidden until judged.** Recall and the briefing skip a distilled memory until the judge has looked at it, so a near
  copy of something already saved never shows twice. `get_memory` by id still returns it.
- **It stays out of the way.** Importance 2 keeps distilled memories out of the briefing's key memories, and below
  explicit memories (importance 3 by default). "Recent decisions" lists explicit decisions first. "Last handoff" is the
  newest handoff, distilled or not.

### Cost

Distillation spends from the judge's own daily budget, `judge.daily_budget_usd`, and may use at most
`distill.budget_share` of it (default 0.5), so it never starves the judge's other work. Its calls are counted on a ledger
row of their own (`jev:distill`); `alice stats` shows "distilled today: N kept, M dropped, $x", and the day's judge total
includes it. A 40-passage request is estimated at about $0.0008, and a typical session takes one or two runs. In the
synthetic evaluation, 30 sessions cost $0.0059 in all, judge included ([benchmarks](benchmarks/sp2-distill.md)).

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
| `waiting_judge` | The judge is not Jev, no Jev key is set, or the Jev judge is still starting |
| `waiting_budget` | Today's share is used up; tried again tomorrow (UTC) |
| `skipped_cloud` | Its project is kept from the cloud; the policy is asked again every 5 minutes |
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
max_per_session_cap = 48    # at most 400: the set grows by 8 per further 100 turns, up to this (below max_per_session: no growth)
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
The daemon reads each file as its sweep does, complete turns only: a file idle for 24 hours (6 hours when its last turn
was left open) is read to its end and its session marked ended, and one written to in the last 5 minutes keeps its last turn open until the sweep closes it. Live
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

**A backfilled session that you resume** is captured either way the agent resumes it:

- If the agent appends to the same transcript, the hook's first pass that stores a new turn makes the session live
  again. What is captured from then on is distilled as any session's is; its backfilled turns, and its backfilled
  subagents', stay undistilled.
- If the agent starts a new transcript that copies the old records first, the copied turns are skipped by their keys,
  and the new transcript is an ordinary live session; the backfilled one keeps `skipped_backfill`.

To distil the backfilled turns too, run `alice capture distill <session-id>` before you resume the session.

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

`alice doctor` has three capture lines. `capture hooks` is always there (a server tenant's doctor leaves it out: the
hooks are on the person's machine); `capture` appears once the daemon has captured something, and `distill` while the
daemon runs.

| Line | What it says | What to do |
|---|---|---|
| `capture hooks` | `not installed` (optional) | `alice hooks install claude` (or `codex`) |
| `capture hooks` | `installed for Claude Code` (and/or `Codex`) | nothing |
| `capture hooks` | the hooks in a file run an `alice` that does not exist, so those sessions are not captured | `alice hooks install claude` (or `codex`) again, from the `alice` you use now |
| `capture` | sessions and chunks captured in the last 24 hours, and the last capture | nothing |
| `capture` | nothing was captured for 7 days, with hooks installed | `alice hooks status claude` and `alice daemon logs`; start a new session |
| `capture` | transcript lines of a shape Alice does not know were skipped | the agent's transcript format may have changed: update Alice |
| `capture` (OK) | bookkeeping lines of the last 7 days were skipped (a type Alice does not know, holding no conversation) | nothing: agents add such lines with new releases |
| `capture` | Codex sessions gave no transcript path and no rollout was found | start the daemon with the `CODEX_HOME` Codex uses |
| `capture` | Codex rollouts are compressed (`.jsonl.zst`) | those sessions are not captured in this version |
| `distill` | `off`, or `jev, N distilled today` | nothing |
| `distill` | distillation waits for the Jev judge (`judge.provider` is not `jev`) | `alice config set judge.provider jev` |
| `distill` | distillation waits for the Jev judge to start | nothing while it starts; otherwise see the `judge` line |
| `distill` | no Jev key is set | `alice config set-secret jev_api_key` |
| `distill` | Jev failed 3 times; tried again at a time | nothing: raw capture goes on, and distillation resumes by itself |
| `distill` | today's distillation share is used up | raise `distill.budget_share` or `judge.daily_budget_usd`, or wait for the next UTC day |

Other things you may see:

| What you see | Meaning | What to do |
|---|---|---|
| Nothing in `alice capture status` after a session | The hooks load in new sessions only, or the daemon was not running | Start a new session; read `$ALICE_HOME/logs/hook.log` |
| A line in `hook.log` | The hook ignored an input or could not reach the daemon (it spooled the notice) | Usually nothing; the daemon reads the transcript when it runs |
| A session's last turn is missing | The trailing turn waits for the next turn, a stop event or 5 quiet minutes; one whose tool call has no result yet waits for the session end, or for 6 hours without a change (on a server tenant, then the next session start) | Wait; it arrives |
| `alice hooks install` refuses a file | `settings.json` (or `hooks.json`) is not valid JSON | Fix the file; nothing was changed |
| `alice hooks install codex` refuses: "could not change … config.toml safely" | Codex's `config.toml` has hooks of its own in a form no table can extend (an inline list), or Alice entries added by hand | Fix what the message names, or add the hooks by hand; nothing was changed |
| Codex captures nothing | The hooks are not trusted yet | Start Codex and choose **Trust all and continue** at "Hooks need review" (or trust the Alice entries in `/hooks`) |
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

`POST <url>/api/v1/capture/transcript?<query>`, with the file's raw bytes from `offset` as the body, at most 8 MiB
(8,388,608 bytes). The tenant script sends `Content-Type: application/x-ndjson`. The server keeps the cursor of each
transcript (its source), and one upload is read as one pass.

| Query | Required | Value |
|---|---|---|
| `agent` | yes | `claude-code` or `codex` |
| `session` | yes | The session id (`^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`) |
| `offset` | yes | The byte offset of the body's first byte in the file (a missing offset is refused, never read as 0) |
| `event` | yes | `session-start`, `stop`, `subagent-stop`, `pre-compact`, `session-end` or `sweep` |
| `subagent` | no | A subagent's id, for its own transcript |
| `project` | no | The repository's folder name, when no `X-Alice-Project` header is sent |
| `more` | no | `1`: the body was cut at a size cap and the transcript goes on past it. Its last turn stays open, and the answer stops at that turn's start; a `session-end` with `more=1` neither closes the turn nor ends the session |
| `no_distill` | no | `1`: store, but never distil (the session shows `skipped_excluded`) |
| `no_capture` | no | `1`: nothing is read or stored, and `next_offset` moves past the body's last complete line |

The optional header `X-Alice-Project` (UTF-8, or percent-encoded UTF-8) names the project and wins over `project`. A local
daemon ignores both and takes the project from a `cwd` query parameter.

**How the offset meets the cursor:**

- **At the cursor:** the body is read. Only complete turns are stored; a turn is complete when the next one starts, or
  at the end of a body without `more=1` (unless its last tool call has no result yet). `session-end` closes the last
  turn whatever it waits for and ends the session.
- **Below the cursor:** nothing is read. The answer is `409 {"next_offset": <cursor>}`: continue from there. This is how
  a client that lost its place, or a first upload from offset 0 after a move, finds the server's cursor.
- **Beyond the cursor (a gap):** the upload is accepted. The cursor jumps to `offset`, the bytes in between are never
  read, and the session's `gaps` count in `GET /api/v1/capture/status` goes up. A client causes one when it skips an
  oversized line (below).
- **An empty `session-end` at the cursor:** when a `stop` already sent everything, send `event=session-end` with an
  empty body at the cursor. The server still ends the session, which makes it due for distillation.

Answers:

- `200 {"next_offset", "turns_stored", "turns_skipped", "chunks_stored", "oversized_line", "ended"}`. Keep
  `next_offset`: the start of the first incomplete turn, or the end of the last complete line. When it equals the
  request's `offset`, nothing was taken in yet (no complete turn, a new subagent transcript still too short to judge, or
  the project's rules are not known right now): wait until the file grows. `"oversized_line": true` means the body is
  one 8 MiB pass with no line end: skip that line and go on after it (a gap). `"ended": true` means this upload ended
  the session; `false` after a `session-end` means the end did not land (the project's rules waited, say), so send it
  again later. A client should treat a missing `ended` as `false`: servers before this release do not send it.
  `"waiting": true` (sent only when set) means the project's capture rules are not known right now, so nothing was
  taken in and nothing ended: send the same bytes again later, even if the file does not grow. The tenant script's
  sweep does so for a close (below); a session that goes on is sent again by its next event.
- `409 {"next_offset": N}`: the server's cursor is ahead of `offset`. Continue from `N`. A cursor beyond the end of the
  local file means the server already holds more of the session than the file has; the tenant script stops that
  session (`status` lists it as `stopped`).
- `400` (a bad query, such as an unknown agent or event or an invalid id), `401` (the token), `413` (a body over 8 MiB:
  send at most 8,388,608 bytes, and the rest from the answer's `next_offset`). On `5xx` or no answer, keep the offset
  and try again at the next event: the file itself is the queue.

The tenant script sends at most 2 MiB at a time, doubles the size (up to 8 MiB) when a capped body held no complete
turn, adds `more=1` whenever a body is as long as its cap, and sends the empty `session-end` at most once per event. At
a session start it also sweeps up to five other transcripts: one that grew while no event sent it, and one the server
took nothing from (a hold) that has not changed for 6 hours, which it sends as `session-end` (the session was left in
the middle of a turn, and its own `SessionEnd` will never come). It sends that close once per hold: again only when
the server did not answer, or answered `"waiting": true`.
