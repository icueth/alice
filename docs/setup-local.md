# Set up Alice on this machine (local)

**For the person.** This file sets up Alice, long-term memory for your AI coding agents, on your own machine, with no
server. Your memory stays in `~/.alice`.

1. Open Claude Code (or Codex) in any folder.
2. Type: `Read docs/setup-local.md from the Alice repository (https://github.com/icueth/alice) and follow every step.`
   If you have a copy of this file, give its path instead.
3. The agent asks you a few questions (whether to install Rust, whether to import MemPalace, whether to retire it, and
   whether to turn on distillation) and does the rest.
4. When it says it is done, restart Claude Code and Codex, so the new MCP server, hooks and instructions load.

To use a team server instead, see [setup-server.md](setup-server.md). To choose, see "Choose: local or server" in the
[README](../README.md).

---

## Instructions for the agent (follow every step, in order)

You are setting up **Alice** locally for the person you are working with. Talk to them in their language. Work carefully;
if a step fails in a way you cannot fix, stop and explain what failed and what they can do.

### Rules (absolute)

- **Never display a secret or a configuration file.** Do not `cat`, `head`, `tail`, `less` or `sed -n` any of these:
  `~/.alice/token`, `~/.alice/secrets.toml`, `~/.claude.json`, `~/.claude/settings.json`, `~/.codex/config.toml`, shell
  profiles (`~/.zshrc`, `~/.bashrc`, `~/.profile`), and any `.env` file. They often hold keys. To check for something,
  use `grep -c` with a fixed pattern (it prints a count, not the line). To append to a file, start with a newline:
  `printf '\n%s\n' "…" >> file`.
- **Never handle a key.** If an API key is needed (Step 8), the person types it into a prompt that hides it. Never ask
  them to paste it into this conversation, and never put one on a command line.
- **Back up before you edit** a file the person owns: `cp <file> <file>.bak-alice`.
- **Never delete MemPalace data**: not the palace directory, not `~/.mempalace`, nothing in it.
- **Ask before:** installing Rust, importing MemPalace, retiring MemPalace, and turning on distillation. Everything else,
  just do.

### Step 0: Preflight

1. Note the system: `uname -s`, `uname -m`, and `echo "$SHELL"`. Alice runs on macOS with Apple Silicon (`Darwin`,
   `arm64`) and on Linux with glibc (`x86_64` or `aarch64`). On anything else, stop and tell the person it is not
   supported yet: macOS on Intel and Linux with musl have no local embedding model yet, and Windows builds but has not
   been tried end to end (the README's "Platform support" table has the details).
2. Which agents exist: `command -v claude` and `command -v codex`. At least one is needed.
3. Whether Alice is already installed: `command -v alice && alice --version`. If it is, skip to Step 2 (Step 1 then only
   updates it, if the person wants that).
4. Whether Rust is installed: `command -v cargo && cargo --version`. Alice needs Rust 1.99 or newer.
5. A C/C++ compiler: on macOS `xcode-select -p` (if it fails, the person runs `xcode-select --install` and confirms the
   dialog); on Debian or Ubuntu `command -v cc` (else `sudo apt install build-essential`, which the person runs).

If Rust is missing or older than 1.99, ask the person: **"Alice is built from source with Rust. Install Rust with
rustup now?"** If yes:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
rustup update stable
```

### Step 1: Build and install

```sh
cargo install --git https://github.com/icueth/alice alice-memory --locked
```

This takes several minutes. It installs the `alice` binary into `~/.cargo/bin`. (From a checkout of the repository,
`cargo install --path crates/alice --locked` does the same.) Then:

```sh
ALICE_BIN=$(command -v alice) && echo "$ALICE_BIN"
alice --version
alice model fetch
```

`ALICE_BIN` must be an absolute path (normally `~/.cargo/bin/alice`). If `command -v alice` finds nothing, add
`~/.cargo/bin` to `PATH` for this shell (`. "$HOME/.cargo/env"`) and try again. Your shell variables may not survive
from one command to the next: set `ALICE_BIN=$(command -v alice)` again in every command below that uses it.
`alice model fetch` downloads the embedding model (about 136 MB) into `~/.alice/models` once.

### Step 2: Run the daemon at login

```sh
alice service install
alice service status
alice status
```

`alice service install` registers a launchd agent (macOS) or a systemd user unit (Linux) that runs the daemon at login,
and starts it. `alice status` must show the daemon running. On Linux, tell the person that user services stop at logout
unless they run `loginctl enable-linger "$USER"` once.

### Step 3: Connect the agents (MCP)

**Claude Code** (only if `claude` exists):

```sh
claude mcp list 2>/dev/null | grep -c '^alice:'
```

If that prints `0`:

```sh
claude mcp add --scope user alice -- "$ALICE_BIN" mcp
```

Then `claude mcp list | grep '^alice:'` must show `alice` as connected. If it does not, retry once after 10 seconds; if
it still fails, report the exact status line. Do not run `claude mcp get alice`.

**Codex** (only if `codex` exists). Do **not** use `codex mcp add`: it rewrites the whole `~/.codex/config.toml` and
drops comments and blocks other tools manage. Back the file up and append one table:

```sh
mkdir -p ~/.codex && touch ~/.codex/config.toml
cp ~/.codex/config.toml ~/.codex/config.toml.bak-alice
if [ "$(grep -c '^\[mcp_servers\.alice\]' ~/.codex/config.toml)" = 0 ]; then
  printf '\n[mcp_servers.alice]\ncommand = "%s"\nargs = ["mcp"]\n' "$ALICE_BIN" >> ~/.codex/config.toml
fi
codex mcp list
```

If a `[mcp_servers.alice]` table already exists, do not edit it: you may not look at this file. Tell the person to open
`~/.codex/config.toml` in their editor and make that table's `command` and `args` lines read like the ones above (with
the path `echo "$ALICE_BIN"` printed), and wait until they say it is done. `codex mcp list` must list `alice`. If Codex
reports a configuration error, restore `~/.codex/config.toml.bak-alice` and tell the person.

### Step 4: Capture sessions automatically

**Claude Code** (only if `claude` exists):

```sh
alice hooks install claude --dry-run
alice hooks install claude
alice hooks status claude
```

The install adds five hooks to `~/.claude/settings.json` (or `$CLAUDE_CONFIG_DIR/settings.json`), keeps every other
hook and setting, and saves the old file as `settings.json.alice-backup-<time>`. If it refuses because the file is not
valid JSON, tell the person; do not edit the file yourself. Note any hooks it lists under "Other hooks mention alice or
mempalace": Step 7 deals with them. The first line of `status` must end in `: installed` (not `: not installed`, and
not `on N of the 5 events only`), and no line under it may say `which does not exist`.

**Codex** (only if `codex` exists):

```sh
alice hooks install codex
alice hooks status codex
```

If it refuses because `~/.codex/config.toml` has hooks of its own, tell the person: Codex accepts hooks in one format
per layer, so those hooks must move into `~/.codex/hooks.json` first. Otherwise tell the person that **Codex runs the
hooks only after they trust them: open Codex, type `/hooks`, and trust the Alice entries.**

What this does: every finished turn of every session is stored in Alice's raw tier, in its project, and each new Claude
Code session starts with a short briefing of the project's memories. Details: [capture.md](capture.md).

### Step 5: Import MemPalace (the person's old memories)

If neither `~/.mempalace` exists nor the person says they used MemPalace, say so and go to Step 6.

Otherwise:

1. Run `alice import mempalace --dry-run`. It reads a snapshot of the palace (MemPalace may keep running) and prints what
   would be imported; it changes nothing. If it cannot find the palace, ask the person for the folder and add
   `--palace <folder>`.
2. Tell the person the counts and ask: **"Import these memories from MemPalace into Alice now? MemPalace's own data is
   not changed or deleted."** If no, go to Step 6.
3. Run `alice import mempalace` (with the same `--palace` if you needed it). It can take a few minutes for a large palace;
   if your command times out, the import goes on in the daemon: check it with `alice import status`.
4. Keep the report's counts for Step 9. Search by words works at once; meaning-based search of the imported transcripts
   fills in over the next hours, in the background.

### Step 6: Tell the agents how to use Alice

Add this block to `~/.claude/CLAUDE.md` (if Claude Code exists) and to `~/.codex/AGENTS.md` (if Codex exists). Create a
file if it is missing. If a block between the same markers is already there (`grep -c '<!-- >>> alice >>> -->'`),
replace it; never add a second one. Do not change anything else in those files.

```markdown
<!-- >>> alice >>> -->
## Alice — long-term memory
- At the start of work in a project, call the Alice `context` tool unless a briefing (`# Context · …`) is already in
  the conversation.
- Before answering about past work, decisions, bugs, people or preferences, call `recall` first. Wrong is worse than slow.
- After meaningful work, `remember` it: decisions (with the reason), bug fixes (symptom, cause, fix), how-tos, warnings,
  facts and preferences. One idea per memory; set `kind`. Before ending a long task, `remember` a `handoff`.
- Sessions are captured automatically as transcripts; do not paste session logs into `remember`.
- Never store secrets (passwords, tokens, keys).
<!-- <<< alice <<< -->
```

### Step 7: Retire MemPalace (optional)

Check whether MemPalace is still wired in, without printing any file:

- an MCP server named like `mempalace` in `claude mcp list` or `codex mcp list`;
- a MemPalace Claude Code plugin: `claude plugin list 2>/dev/null | grep -c -i mempalace`;
- MemPalace hooks, or an old Alice SessionStart script, in `~/.claude/settings.json`: the ones `alice hooks install`
  listed in Step 4 (or `grep -c -i mempalace ~/.claude/settings.json`);
- "MemPalace Protocol" blocks in `~/.claude/CLAUDE.md` or `~/.codex/AGENTS.md`: `grep -c -i 'mempalace' <file>`.

If none, skip this step. If some exist, ask the person once: **"Stop using MemPalace and use Alice instead
(recommended)? Its data is kept, not deleted."**

- **Yes:**
  1. Back up each file you change (`cp <file> <file>.bak-alice`).
  2. Disable the MemPalace plugin: `claude plugin disable <name>` (or the person types `/plugin` in Claude Code). That
     stops its hooks and its MCP server.
  3. Remove any other MemPalace MCP server entries of Claude Code (`claude mcp remove <name> -s user`). A MemPalace
     `[mcp_servers.<name>]` table in Codex's `config.toml` is the person's to delete, in their editor: you may not look
     at that file.
  4. The MemPalace hook entries, and any old SessionStart script that ran `alice context` or `alice import mempalace`,
     in `~/.claude/settings.json` (the ones Step 4 listed): ask the person to remove only those, with `/hooks` in
     Claude Code, keeping every other hook. You may not look at that file, so do not edit it yourself. Wait until they
     say it is done.
  5. Remove the "MemPalace Protocol" blocks (diaries every few tool uses) from `CLAUDE.md` / `AGENTS.md`. Capture makes
     them redundant.
  6. Keep Alice's MemPalace compatibility on, so old prompts that call `mempalace_*` tools keep working:
     `alice config set compat.mempalace true`. If the person's prompts call tools by their full Claude Code name
     (`mcp__plugin_mempalace_mempalace__…`), register Alice under that server name **instead of** `alice`. One entry
     serves Alice's tools and the `mempalace_*` ones; two would list Alice's tools twice:

     ```sh
     claude mcp remove alice -s user
     claude mcp add --scope user plugin_mempalace_mempalace -- "$ALICE_BIN" mcp
     ```
  7. If Step 5 imported MemPalace earlier than this step, run `alice import mempalace` once more now, so nothing it
     caught in between is lost. Alice never stores a session's turns twice.
- **No:** change nothing; tell the person both systems will run and the agents may use either.

**Never delete the palace directory, `~/.mempalace`, or any MemPalace data.**

### Step 8: Distillation (optional)

Distillation turns ended sessions into a few short memories (decisions, fixes, how-tos and so on) with TypeSafe's Jev
model. It sends session text, with secrets removed and only for projects the person allows, to that cloud service. It
costs about 0.1 to 0.3 US cents per session, and never more than half of the judge's daily budget (1 USD a day by
default). It is off unless the person turns it on. Ask: **"Turn on distillation with Jev? You need a TypeSafe API
key."**

If yes, ask the person to run this **themselves, in their own terminal** (not through you), and paste the key at the
prompt, which hides it:

```sh
alice config set-secret jev_api_key
```

When they say it is done:

```sh
alice config set judge.provider jev
alice config set distill.provider jev
alice doctor
```

The `distill` line of `alice doctor` must say `jev`. If it says no key is set, the key step did not work; ask the person
to run it again.

### Step 9: Check it works

```sh
alice doctor
alice hooks status claude      # and/or: alice hooks status codex
alice context
```

`alice doctor` exits 1 when a check fails; read every line that is not `OK` and fix what it says, or report it. `alice
context` must print a briefing (it may say there are no memories yet if nothing was imported).

### Step 10: Report to the person

Say briefly:

- which agents were connected, and that capture is installed for them;
- the import's counts, or why it was skipped;
- what changed in MemPalace, and that its data was kept;
- whether distillation is on;
- that they must **restart Claude Code and Codex** so the MCP server, the hooks and the instructions load, and, for
  Codex, **trust the Alice hooks in `/hooks`**;
- how to use it: work as usual; the agents recall and save by themselves, and every session is captured.
  `alice recall --raw "<words>"` searches the captured transcripts, and `alice hooks uninstall claude` (or `codex`)
  removes capture again.
