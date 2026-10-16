# Set up Alice with your team's server (tenant)

**For the person.** Your team runs Alice, long-term memory for AI coding agents, on a shared server, and your admin has
made you a personal space on it (a "tenant"). This file connects your machine to it. Nothing heavy is installed: your
agents talk to the server over HTTPS.

Your admin gives you two things:

- **your address**, such as `https://alice.example/u/<name>` (yours has the team's host and your name);
- **your token**, usually as a file named `alice-token`. It is the key to your memory: never send it to anyone, and you
  do not need to open it.

Then:

1. Put `alice-token` in your Downloads folder (or anywhere; you will tell the agent where).
2. Open Claude Code (or Codex) in any folder and type:
   `Read docs/setup-server.md from the Alice repository (https://github.com/icueth/alice) and follow every step. My
   address is https://…/u/<name> and my token file is ~/Downloads/alice-token.`
3. The agent asks you one or two questions and does the rest.
4. When it says it is done, restart Claude Code and Codex (and open a new terminal for Codex).

To run Alice on your own machine instead, see [setup-local.md](setup-local.md).

---

## Instructions for the agent (follow every step, in order)

You are connecting the person's agents to their **Alice tenant** on a team server. Talk to them in their language. Work
carefully; if a step fails in a way you cannot fix, stop and explain what failed.

### Facts

- **`<url>`** below is the person's tenant address: `https://<host>/u/<name>`, with no trailing slash and without
  `/mcp`. After Step 1 it is in `~/.config/alice/url`, and the commands read it from there with
  `$(head -n 1 ~/.config/alice/url)`.
- The MCP endpoint is `<url>/mcp` (Streamable HTTP, bearer token).
- The token goes in `~/.config/alice/token` (mode 0600, one line). The capture setup, the MemPalace upload and the MCP
  helpers below all read it from there, each time they need it. Nothing exports it into a shell, so an environment dump
  (`env`, `printenv`, a tool's error report) never shows it.

### The token rule (absolute)

**Never print, `cat`, `echo`, display, quote or paste the token**, and never put it in a commit, a chat message, a
command line or a command's visible output. Use it only through the exact command substitutions below
(`$(cat ~/.config/alice/token)` inside `printf`), which keep it out of this conversation. Never `export` it or put it
in a shell profile. Do not run `claude mcp get alice`.

**Never display the contents of configuration files**: `~/.zshrc`, `~/.bashrc`, `~/.profile`, `~/.codex/config.toml`,
`~/.claude.json`, `~/.claude/settings.json`, `~/.config/alice/*`, and any `.env` file. They often hold other secrets. Do
not `cat`, `head`, `tail`, `less` or `sed -n` them. To check for something, use `grep -c` with a fixed pattern (it prints
a count, not the line). To append, always start with a newline: `printf '\n%s\n' "…" >> file`. Back up a file before you
change it: `cp <file> <file>.bak-alice`.

### Step 0: Preflight

1. Note the system and shell: `uname -s`, `echo "$SHELL"`. macOS and Linux are supported (the capture script needs a
   POSIX shell).
2. Which agents exist: `command -v claude`, `command -v codex`. At least one is needed. `command -v curl` must succeed.
3. The address: if `~/.config/alice/url` exists, use its first line as `<url>`. Otherwise take it from the person (ask if
   they did not give it). It must start with `https://`; drop a trailing `/` or `/mcp`.
4. The token: if `~/.config/alice/token` exists and is not empty (`test -s`), it is already installed; do not read it.
   Otherwise find the file the person named (`test -f <path>`), without reading it. If they have no file, ask them to
   create it **themselves, in their own terminal**, by pasting the token at a hidden prompt:

   ```sh
   mkdir -p ~/.config/alice && chmod 700 ~/.config/alice
   (umask 077; read -rs T && printf '%s\n' "$T" > ~/.config/alice/token)
   ```

5. The server answers: `curl -fsS "<url>/api/v1/ping"` must succeed. It needs no token.

### Step 1: Install the token and the address

When the token is not installed yet (replace `<token file>` with the person's file and `<url>` with the address):

```sh
mkdir -p ~/.config/alice && chmod 700 ~/.config/alice
install -m 600 "<token file>" ~/.config/alice/token
```

Then, in every case:

```sh
test -f ~/.config/alice/url || { printf '%s\n' "<url>" > ~/.config/alice/url && chmod 600 ~/.config/alice/url; }
chmod 600 ~/.config/alice/token
printf 'header = "Authorization: Bearer %s"\n' "$(cat ~/.config/alice/token)" \
  | curl -q -fsS -K - "$(head -n 1 ~/.config/alice/url)/api/v1/stats" -o /dev/null -w '%{http_code}\n'
```

The last command must print `200`. If it prints `401`, the token is wrong: stop and tell the person to ask the admin.
If you installed it from a file, ask the person whether to delete that file now (the installed copy is the one in use),
and run `rm "<token file>"` only if they say yes.

### Step 2: Claude Code (only if `claude` exists)

Claude Code gets its headers from a small helper script, so the token never sits in Claude Code's configuration, and
Alice learns which project (git repository) each session is in. Claude Code runs helpers from `~/.claude`, not from the
session's folder, so the helper reads the session's folder from its parent process (Claude Code) with `lsof`.

First, if `claude mcp list 2>/dev/null | grep -c '^alice:'` prints more than `0`, an `alice` entry already exists (it may
be a local Alice). Ask the person once: **"Replace your current `alice` MCP entry with the team server?"** If no, skip
this step. Then write the helper exactly as below (an existing helper is kept as `alice-mcp-headers.bak-alice`):

```sh
mkdir -p ~/.local/bin
[ -e ~/.local/bin/alice-mcp-headers ] && cp ~/.local/bin/alice-mcp-headers ~/.local/bin/alice-mcp-headers.bak-alice
cat > ~/.local/bin/alice-mcp-headers <<'EOF'
#!/bin/sh
# Claude Code MCP headersHelper for Alice: the bearer token from ~/.config/alice/token and, inside a
# git repository, X-Alice-Project = that repository's folder name. Claude Code runs helpers from
# ~/.claude, so the session's folder is read from the parent (claude) process; when it cannot be
# read, no project is sent.
tok=$(cat "$HOME/.config/alice/token" 2>/dev/null) || exit 1
[ -n "$tok" ] || exit 1
dir=$(lsof -a -p "$PPID" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | head -n 1)
if [ -n "$dir" ] && top=$(git -C "$dir" rev-parse --show-toplevel 2>/dev/null) && [ "$top" != "$HOME" ]; then
  proj=$(basename "$top" | tr -d '"\\')
  # A name that is not plain ASCII (a Thai folder name), or holds %, is sent percent-encoded.
  if printf '%s' "$proj" | LC_ALL=C grep -Eq '[^ -~]|%'; then
    proj=$(printf '%s' "$proj" | LC_ALL=C od -An -tx1 -v | tr -d ' \n' | sed 's/../%&/g')
  fi
  printf '{"Authorization":"Bearer %s","X-Alice-Project":"%s"}\n' "$tok" "$proj"
else
  printf '{"Authorization":"Bearer %s"}\n' "$tok"
fi
exit 0
EOF
chmod 755 ~/.local/bin/alice-mcp-headers
claude mcp list 2>/dev/null | grep -q '^alice:' && claude mcp remove alice -s user
claude mcp add-json alice "{\"type\":\"http\",\"url\":\"$(head -n 1 ~/.config/alice/url)/mcp\",\"headersHelper\":\"$HOME/.local/bin/alice-mcp-headers\"}" -s user
claude mcp list | grep '^alice:'
```

The last line must show `alice` as connected. If it does not, retry once after 10 seconds; if it still fails, report
the exact status line. On Linux without `lsof` the helper still works, but no project is sent, so every session files
its memories under `global`; installing `lsof` fixes that. <!-- TODO(confirm): the percent-encoding lines are new
beside the proven helper (ASCII names are sent as before). The server's `project_header` decodes any %xx, in either
case, and requires UTF-8, for MCP and capture alike, and the review ran the lines under sh, dash, bash and zsh with
the same result; not yet tried end to end with a real Claude Code and a Thai folder. -->

### Step 3: Codex (only if `codex` exists)

Do **not** use `codex mcp add`: it rewrites the whole `~/.codex/config.toml` and drops comments and blocks other tools
manage. Back the file up and append one table:

```sh
mkdir -p ~/.codex && touch ~/.codex/config.toml
cp ~/.codex/config.toml ~/.codex/config.toml.bak-alice
if [ "$(grep -c '^\[mcp_servers\.alice\]' ~/.codex/config.toml)" = 0 ]; then
  printf '\n[mcp_servers.alice]\nurl = "%s"\nbearer_token_env_var = "ALICE_TOKEN"\nenv_http_headers = { "X-Alice-Project" = "ALICE_PROJECT" }\n' \
    "$(head -n 1 ~/.config/alice/url)/mcp" >> ~/.codex/config.toml
fi
```

If a `[mcp_servers.alice]` table already exists, do not edit it: you may not look at this file. Tell the person to open
`~/.codex/config.toml` in their editor and make that table read exactly like the four lines above (with their address
in `url`), and wait until they say it is done. Codex reads the token from `ALICE_TOKEN` and the project from
`ALICE_PROJECT`; the wrapper below sets both for Codex alone.

Add this marked block to the person's shell profile (`~/.zshrc` for zsh, `~/.bashrc` for bash; create it if missing). If
`grep -c '# >>> alice >>>'` finds an old block, replace everything between the markers (an older block exported
`ALICE_TOKEN`; this one does not); otherwise append it with a leading newline:

```sh
# >>> alice >>>
# Codex gets the token from $ALICE_TOKEN and sends X-Alice-Project from $ALICE_PROJECT (the git
# repository folder Codex starts in). Both are set for Codex alone: nothing is exported to the shell.
codex() {
  local top name
  if top=$(git rev-parse --show-toplevel 2>/dev/null) && [ "$top" != "$HOME" ]; then
    name=${top##*/}
    if printf '%s' "$name" | LC_ALL=C grep -Eq '[^ -~]|%'; then
      name=$(printf '%s' "$name" | LC_ALL=C od -An -tx1 -v | tr -d ' \n' | sed 's/../%&/g')
    fi
    ALICE_TOKEN="$(cat "$HOME/.config/alice/token" 2>/dev/null)" ALICE_PROJECT="$name" command codex "$@"
  else
    ALICE_TOKEN="$(cat "$HOME/.config/alice/token" 2>/dev/null)" command codex "$@"
  fi
}
# <<< alice <<<
```

The token is read from its file each time Codex starts, so other programs started from the shell (an agent's tool
commands among them) never see it. Codex itself leaves variables named like `*TOKEN*` out of the commands it runs, by
default (its `shell_environment_policy`). The wrapper exists only in a shell that reads this profile: a Codex started
some other way, such as from an editor, gets neither the token nor the project, so tell the person to start Codex from a
terminal.

Check with a fresh shell, without printing values: `zsh -ic 'type codex | head -1'` (or `bash -ic` for bash) must say
that `codex` is a shell function. Then `codex mcp list` must list `alice`. If Codex reports a configuration error,
restore `~/.codex/config.toml.bak-alice` and tell the person. <!-- TODO(confirm): the percent-encoding lines are new
beside the proven wrapper; the review ran them under sh, dash, bash and zsh, not yet with a real Codex and a Thai
folder. -->

### Step 4: Upload the person's MemPalace (their old memories)

1. If neither `~/.mempalace` exists nor the person says they have MemPalace elsewhere, say so and go to Step 5. If it
   lives somewhere unusual, ask for the palace folder.
2. Find the palace folder (`palace_path` in `~/.mempalace/config.json`, if present; `grep` for the key, do not print the
   file) and its size (`du -sh <folder>`). Ask once: **"Upload your old memories from MemPalace (<folder>, <size>) to
   your space on the team server now?"** If no, go to Step 5.
3. Ask the person to close other agent sessions that use MemPalace for the next few minutes. The script takes a safe
   snapshot when `sqlite3` is installed, but an idle palace is best.
4. Run it. The script finds the palace, packs a snapshot, uploads it with the token from `~/.config/alice/token`, and
   waits while the server imports it. It is downloaded into a new private temporary folder, which is removed afterwards
   (a fixed name in a shared `/tmp` could be swapped for another user's file before it runs):

   ```sh
   (
     d=$(mktemp -d "${TMPDIR:-/tmp}/alice-upload.XXXXXX") || exit 1
     trap 'rm -rf "$d"' EXIT
     trap 'exit 1' HUP INT TERM
     url=$(head -n 1 ~/.config/alice/url)
     curl -fsS "$url/api/v1/import/upload.sh" -o "$d/upload.sh" && sh "$d/upload.sh" "$url"
   )
   ```

   Add `--palace <folder>` (and `--kg <file>` for a knowledge graph elsewhere) after `"$url"` when it was not found by
   itself.
5. If downloading `upload.sh` fails with HTTP 404, this address does not serve the self-service upload: the server
   predates it, or the address is not a tenant's (Step 1 would have failed for a wrong name). Tell the person to ask
   the admin, skip the upload, and go on.
6. Keep the report's counts for Step 9. A big palace can take a long time on the server. If your command times out, the
   import goes on there: run the script again later and it says whether one is still running. Word search works at once;
   meaning-based search fills in over the next hours.

### Step 5: Tell the agents how to use Alice

Add this block to `~/.claude/CLAUDE.md` (if Claude Code exists) and to `~/.codex/AGENTS.md` (if Codex exists). Create a
file if it is missing. If a block between the same markers already exists, replace it; never add a second one. Do not
change anything else in those files.

```markdown
<!-- >>> alice >>> -->
## Alice — long-term memory (team server)
- At the start of work in a project, call the Alice `context` tool unless a memory briefing is already in the conversation.
- Before answering about past work, decisions, bugs, people or preferences, call `recall` first. Wrong is worse than slow.
- After meaningful work, `remember` it: decisions (with the reason), bug fixes (symptom, cause, fix), how-tos, warnings,
  facts and preferences. One idea per memory; set `kind`. Before ending a long task, `remember` a `handoff`.
- When the user says "save this to shared memory" (in Thai: "บันทึกลงความจำส่วนกลาง"), call `remember` with
  `shared: true` so the whole team can find it. Everything else stays private to this person.
- If a briefing says `global` while you are inside a git repository, pass `project: <repository folder name>` to
  `context`, `recall` and `remember`.
- Never store secrets (passwords, tokens, keys).
<!-- <<< alice <<< -->
```

### Step 6: Capture sessions automatically

This saves every Claude Code and Codex session to the person's tenant as it happens (each finished turn, in the
session's project), and starts each new Claude Code session with the project's briefing. It reuses
`~/.config/alice/token` and `~/.config/alice/url`, so it asks nothing. As in Step 4, the setup script is downloaded
into a new private temporary folder that is removed afterwards:

```sh
(
  d=$(mktemp -d "${TMPDIR:-/tmp}/alice-setup.XXXXXX") || exit 1
  trap 'rm -rf "$d"' EXIT
  trap 'exit 1' HUP INT TERM
  url=$(head -n 1 ~/.config/alice/url)
  curl -fsSL "$url/api/v1/capture/setup.sh" -o "$d/setup.sh" && sh "$d/setup.sh" "$url" </dev/null
)
sh ~/.config/alice/alice-capture.sh status
```

`status` must say `server: reachable, token accepted` and list the plugin `alice-capture@alice-local`.

- If setup printed two `/plugin …` commands instead of installing the plugin, tell the person to type them inside Claude
  Code.
- If Codex is installed, setup wrote `~/.codex/hooks.json` (or printed the entries to add, when the file already
  existed: add them without disturbing the rest, after a backup). Tell the person that **Codex runs the hooks only after
  they trust them: open Codex, type `/hooks`, and trust the Alice entries.**
- If setup says the token was refused, go back to Step 1. If it says the address is "a local Alice daemon, not a server
  tenant", the address is wrong.
- **The hook script does not update itself.** Setup downloads it once, into `~/.config/alice/alice-capture.sh`. When the
  admin upgrades the server with changed capture scripts, the person runs the same setup line again (or asks an agent to
  repeat this step). It is safe to repeat: it reuses `~/.config/alice/token` and `~/.config/alice/url` without asking,
  keeps their briefing choice, and replaces the script and the plugin with the server's current ones.

Details of what it installed and how to remove it: [server.md, "Capture for team members"](server.md#capture-for-team-members).

### Step 7: MemPalace, if the person has it

Check whether MemPalace is still wired in: an MCP server named like `mempalace` in `claude mcp list` or `codex mcp
list`, a MemPalace Claude Code plugin (`claude plugin list`), MemPalace hooks in `~/.claude/settings.json`
(`grep -c -i mempalace`), or "MemPalace" protocol blocks in `~/.claude/CLAUDE.md` / `~/.codex/AGENTS.md`. If none, skip
this step.

If some exist, ask the person once: **"Stop using MemPalace and use Alice instead (recommended)? Your old MemPalace data
is not deleted."**

- **Yes:** back up each file you change (`cp <file> <file>.bak-alice`), then:
  - disable the MemPalace plugin (`claude plugin disable <name>`), which also stops its hooks and its MCP server;
  - remove other MemPalace MCP servers of Claude Code with `claude mcp remove <name> -s user`;
  - for what lives in files you may not look at, hand the step to the person: a MemPalace `[mcp_servers.<name>]` table in
    `~/.codex/config.toml` (they delete that table in their editor), and MemPalace hook entries left in
    `~/.claude/settings.json` (they remove only those, with `/hooks` in Claude Code). Wait until they say it is done;
  - remove MemPalace protocol blocks from `CLAUDE.md` / `AGENTS.md`.

  **Never delete the palace directory, `~/.mempalace`, or any MemPalace data.**
- **No:** change nothing; tell the person both systems will run and agents may use either.

### Step 8: Check it works

Over HTTP, without printing the token:

```sh
printf 'header = "Authorization: Bearer %s"\n' "$(cat ~/.config/alice/token)" \
  | curl -q -fsS -K - -X POST "$(head -n 1 ~/.config/alice/url)/api/v1/context" -H 'content-type: application/json' -d '{}' \
  | head -c 600; echo
```

It must return a briefing (it may say there are no memories yet if nothing was imported).

### Step 9: Report to the person

Say briefly:

- which agents were connected, and that their sessions are now captured;
- the import's counts, or why it was skipped;
- what changed in MemPalace, and that its data was kept;
- that they must **restart Claude Code and Codex** (and open a new terminal for Codex, started from that terminal) so
  the new MCP server, hooks and instructions load, and **trust the Alice hooks in Codex's `/hooks`**;
- how to use it: work as usual; the agents recall and save by themselves. To share something with the team, say "save
  this to shared memory";
- that `sh ~/.config/alice/alice-capture.sh status` shows the capture, and `sh ~/.config/alice/alice-capture.sh
  uninstall` removes it (the token and address stay);
- that the capture script does not update itself: after the admin announces a server upgrade, they run the setup line of
  Step 6 again (it asks nothing).

Do not show the token or any file's contents.
