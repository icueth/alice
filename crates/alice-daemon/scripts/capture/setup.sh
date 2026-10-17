#!/bin/sh
# Alice: capture this machine's Claude Code (and Codex) sessions into your Alice server tenant
# (SP2 spec section 5.4). Run the one line your admin gave you:
#
#   curl -fsSL https://<host>/u/<name>/api/v1/capture/setup.sh | sh -s -- https://<host>/u/<name>
#
# It reuses ~/.config/alice/url and ~/.config/alice/token when they exist (the team's setup
# document writes them) and asks for the token only when the file is missing or the server
# refuses it. Then it installs the hook script (~/.config/alice/alice-capture.sh) and a small
# Claude Code plugin from a local marketplace (~/.config/alice/claude-marketplace). The hook
# script then updates itself from the tenant once a day, with the plugin's hooks.json when that
# changes. Running this again repairs an install in place;
# `sh ~/.config/alice/alice-capture.sh uninstall` removes it.
# When `codex` is installed it also writes $CODEX_HOME/hooks.json (default ~/.codex/hooks.json)
# if that file does not exist. When $CODEX_HOME/config.toml has hooks of its own (Codex reads
# hooks from one format per layer), the hooks go into a marked block at the end of config.toml
# instead, after a backup to config.toml.bak-alice, when python3 can check the edit; otherwise
# the entries are printed. Codex runs a new hook only after the person trusts it: the next time
# Codex opens it asks "Hooks need review", and the person chooses "Trust all and continue". That
# trust is the person's decision; this script never writes it.
#
# The token is read from the terminal with echo off, never from argv or the environment
# (ALICE_SETUP_TOKEN_STDIN=1 reads it from stdin instead, for tests). It is checked with the
# server before anything is written, kept in ~/.config/alice/token only, and reaches curl only
# as a config line on curl's stdin (curl -q -K -), written by the printf builtin.
# POSIX sh; needs curl. Never run it with set -x: that would print the token.

set -u
umask 077

TMPD=
TOKEN_TMP=
HOOK_TMP=
HOOK_WHY=
ECHO_OFF=
CODE=
CHECK_ERR=
token=

say() {
  printf '%s\n' "$*"
}

die() {
  printf 'alice setup: %s\n' "$*" >&2
  exit 1
}

cleanup() {
  if [ -n "$ECHO_OFF" ]; then
    stty echo </dev/tty 2>/dev/null
    ECHO_OFF=
  fi
  if [ -n "$TOKEN_TMP" ]; then rm -f "$TOKEN_TMP"; fi
  if [ -n "$HOOK_TMP" ]; then rm -f "$HOOK_TMP"; fi
  if [ -n "$TMPD" ]; then rm -rf "$TMPD"; fi
}

valid_token() {
  case $1 in '' | *[!A-Za-z0-9._~+/=-]*) return 1 ;; esac
  [ ${#1} -ge 16 ] && [ ${#1} -le 1024 ]
}

# Where the token may go: https://, or http:// to 127.0.0.1 or localhost (tests).
valid_url() {
  case $1 in *[!A-Za-z0-9._~:/%-]*) return 1 ;; esac
  case $1 in
    https://[A-Za-z0-9]*) return 0 ;;
    http://127.0.0.1 | http://127.0.0.1/* | http://localhost | http://localhost/*) return 0 ;;
    http://127.0.0.1:* | http://localhost:*)
      vu_port=${1#http://*:}
      vu_port=${vu_port%%/*}
      case $vu_port in '' | *[!0-9]*) return 1 ;; esac
      return 0
      ;;
  esac
  return 1
}

# Writes the line $2 to file $1 (whole, then moved into place; 0600 by the umask).
write_file() {
  printf '%s\n' "$2" >"$1.tmp.$$" && mv -f "$1.tmp.$$" "$1" || die "cannot write $1"
}

# Asks for the token once: from /dev/tty with echo off, or from stdin for tests.
ask_token() {
  token=
  if [ "${ALICE_SETUP_TOKEN_STDIN:-}" = 1 ]; then
    printf 'Alice token for %s: ' "$url" >&2
    IFS= read -r token || :
    printf '\n' >&2
  else
    (exec </dev/tty) 2>/dev/null ||
      die "there is no terminal to ask for the token on; put it in ~/.config/alice/token (one line, mode 0600) and run the command again"
    if stty -echo </dev/tty 2>/dev/null; then
      ECHO_OFF=1
    else
      printf 'Warning: this terminal cannot hide what you type; the token will show on the screen.\n' >/dev/tty
    fi
    if [ -n "$ECHO_OFF" ]; then
      printf 'Alice token for %s (typing is hidden): ' "$url" >/dev/tty
    else
      printf 'Alice token for %s: ' "$url" >/dev/tty
    fi
    IFS= read -r token </dev/tty || :
    if [ -n "$ECHO_OFF" ]; then
      stty echo </dev/tty 2>/dev/null
      ECHO_OFF=
    fi
    printf '\n' >/dev/tty
  fi
  token=$(printf '%s' "$token" | tr -d ' \t\r')
}

# Checks $token with GET <url>/api/v1/capture/status. CODE: ok (a tenant took it), refused
# (401), or error (with CHECK_ERR). The answer stays in $TMPD/status.
check_token() {
  rm -f "$TMPD/status"
  ct_code=$(printf 'header = "Authorization: Bearer %s"\n' "$token" | curl -q -K - -fsS --connect-timeout 5 --max-time 30 \
    -o "$TMPD/status" -w '%{http_code}' "$url/api/v1/capture/status" 2>"$TMPD/curl.err")
  ct_rc=$?
  if [ "$ct_code" = 200 ] && grep -q '"tenant"[[:space:]]*:[[:space:]]*true' "$TMPD/status" 2>/dev/null; then
    CODE=ok
    return 0
  fi
  if [ "$ct_code" = 401 ]; then
    CODE=refused
    return 0
  fi
  CODE=error
  case $ct_code in
    200) CHECK_ERR="$url is a local Alice daemon, not a server tenant; this setup is for server tenants (a local install uses alice hooks install)" ;;
    000 | '') CHECK_ERR="could not reach $url (curl exit $ct_rc: $(head -n 1 "$TMPD/curl.err" 2>/dev/null)); check the address and your connection, then run the command again" ;;
    *) CHECK_ERR="$url answered HTTP $ct_code when the token was checked; check the address, then run the command again" ;;
  esac
}

marketplace_json() {
  printf '{\n'
  printf '  "name": "alice-local",\n'
  printf '  "owner": { "name": "Alice" },\n'
  printf '  "plugins": [\n'
  printf '    {\n'
  printf '      "name": "alice-capture",\n'
  printf '      "source": "./alice-capture",\n'
  printf '      "description": "Sends your Claude Code transcripts to your Alice server and briefs each new session.",\n'
  printf '      "version": "%s"\n' "$version"
  printf '    }\n'
  printf '  ]\n'
  printf '}\n'
}

plugin_json() {
  printf '{\n'
  printf '  "name": "alice-capture",\n'
  printf '  "version": "%s",\n' "$version"
  printf '  "description": "Sends your Claude Code transcripts to your Alice server and briefs each new session."\n'
  printf '}\n'
}

# Whether file $1 is a whole hook script: #!/bin/sh, its marker and version lines, `main "$@"`
# last, and it parses (sh -n, and dash -n where dash exists). HOOK_WHY says what is wrong.
check_hook() {
  HOOK_WHY=
  if [ "$(head -n 1 "$1")" != '#!/bin/sh' ]; then
    HOOK_WHY="it does not start with #!/bin/sh"
  elif ! sed -n 2p "$1" | grep -q '^# alice-capture-hook'; then
    HOOK_WHY="it has no alice-capture-hook marker line"
  elif ! head -n 5 "$1" | grep -q '^# alice-capture-version: [A-Za-z0-9._+-]'; then
    HOOK_WHY="it has no version line"
  elif [ "$(tail -n 1 "$1")" != 'main "$@"' ]; then
    HOOK_WHY="it is cut short"
  elif ! sh -n "$1" 2>/dev/null; then
    HOOK_WHY="it fails sh -n"
  elif command -v dash >/dev/null 2>&1 && ! dash -n "$1" 2>/dev/null; then
    HOOK_WHY="it fails dash -n"
  fi
  [ -z "$HOOK_WHY" ]
}

# The hooks file for agent $1 (claude-code: the plugin's hooks.json; codex: $CODEX_HOME/hooks.json),
# as the installed hook script prints it: the script is the one place the hooks are written down,
# and its daily update compares with the same text.
hooks_json() {
  sh "$DIR/alice-capture.sh" print-hooks-json "$1" </dev/null 2>/dev/null
}

# Whether config.toml ($1) has hooks of its own: a top-level `hooks` key, the one rule of
# `alice hooks install codex` (agent_hooks/codex_toml.rs, has_hooks). That is a `[hooks…]` or
# `[[hooks…]]` table header, or a `hooks = …` / `hooks.… = …` key before the first header.
# `[features] hooks = true` is not one (final review A-M7).
codex_inline_hooks() {
  ci_sp='[[:space:]]'
  grep -Eq "^${ci_sp}*\\[\\[?${ci_sp}*[\"']?hooks[\"']?${ci_sp}*[].]" "$1" 2>/dev/null && return 0
  sed -n "/^${ci_sp}*\\[/q;p" "$1" 2>/dev/null | grep -Eq "^${ci_sp}*[\"']?hooks[\"']?${ci_sp}*[.=]"
}

# Codex (spec section 5.4 step 8). hooks.json is written only when it does not exist, or is
# still exactly the file this script wrote earlier (then it is updated in place); a file of the
# person's own is never edited: the entries are printed instead (or, when they are in the file
# already, said to be there). When config.toml has hooks of its own, the hooks go there instead
# (codex_toml_setup).
#
# The record of what was written ($DIR/codex-hooks.written) is staged first, so a record that
# cannot be written leaves hooks.json untouched, and is reported as that.
codex_setup() {
  command -v codex >/dev/null 2>&1 || return 0
  cs_home="${CODEX_HOME:-$HOME/.codex}"
  cs_file="$cs_home/hooks.json"
  cs_new=$(hooks_json codex)
  case $cs_new in
    '{'*) ;;
    *)
      say ""
      say "Codex: the hook script printed no hooks file, so nothing was written for Codex; run the command again."
      cs_state="not written"
      return 0
      ;;
  esac
  cs_re='alice-capture\.sh.{1,3}hook codex'
  cs_write=
  cs_why=
  cs_have=
  cs_existed=
  if [ -f "$cs_file" ]; then
    cs_existed=1
    if [ -f "$DIR/codex-hooks.written" ] && cmp -s "$cs_file" "$DIR/codex-hooks.written"; then
      cs_write=1
    else
      cs_why="$cs_file already exists"
      if grep -Eq "$cs_re" "$cs_file" 2>/dev/null; then cs_have=1; fi
    fi
  elif [ -e "$cs_file" ]; then
    cs_why="$cs_file exists and is not a file"
  elif codex_inline_hooks "$cs_home/config.toml"; then
    say ""
    codex_toml_setup "$cs_home/config.toml"
    return 0
  else
    cs_write=1
  fi
  say ""
  if [ -n "$cs_write" ]; then
    cs_stage="$DIR/codex-hooks.written.new"
    rm -f "$cs_stage" 2>/dev/null
    if ! { printf '%s\n' "$cs_new" >"$cs_stage"; } 2>/dev/null; then
      rm -f "$cs_stage" 2>/dev/null
      say "Codex: could not write the record $DIR/codex-hooks.written, so $cs_file was not written and was not changed; add these entries to your hooks yourself:"
      say "$cs_new"
      cs_state="not written"
      return 0
    fi
    if ! { mkdir -p "$cs_home" && printf '%s\n' "$cs_new" >"$cs_file.tmp.$$" && mv -f "$cs_file.tmp.$$" "$cs_file"; } 2>/dev/null; then
      rm -f "$cs_file.tmp.$$" "$cs_stage" 2>/dev/null
      say "Codex: could not write $cs_file; add these entries to it yourself:"
      say "$cs_new"
      cs_state="not written"
      return 0
    fi
    if ! mv -f "$cs_stage" "$DIR/codex-hooks.written" 2>/dev/null; then
      rm -f "$cs_stage" 2>/dev/null
      if [ -z "$cs_existed" ]; then rm -f "$cs_file" 2>/dev/null; fi
      say "Codex: could not write the record $DIR/codex-hooks.written, so $cs_file was not left as written; add these entries to your hooks yourself:"
      say "$cs_new"
      cs_state="not written"
      return 0
    fi
    say "Codex: wrote $cs_file with the five Alice capture hooks."
    cs_state="installed"
  elif [ -n "$cs_have" ]; then
    say "Codex: $cs_why and already has the Alice capture entries; it was left as it is."
    cs_state="entries present"
  else
    say "Codex: $cs_why; it was left as it is. Add these hook entries to your hooks:"
    say "$cs_new"
    cs_state="entries printed"
  fi
}

# Codex's config.toml ($1) has hooks of its own (Codex reads hooks from one format per layer, so
# no hooks.json is added beside them): the hook script appends Alice's block to it (codex-config
# install; a block already there is left as it is, or repaired in place), after a backup to
# config.toml.bak-alice. When it cannot check the edit (no python3 with a TOML reader), or the
# result would not read back as meant, config.toml is left as it is and the block is printed.
codex_toml_setup() {
  ct_how=$(sh "$DIR/alice-capture.sh" codex-config install </dev/null 2>/dev/null)
  ct_rc=$?
  case $ct_rc in
    10)
      if [ "$ct_how" = replaced ]; then
        say "Codex: repaired Alice's block of capture hooks in $1. The previous file is $1.bak-alice."
      else
        say "Codex: $1 has hooks of its own (Codex reads one format per layer), so the five Alice capture hooks were added to it, in a marked block at its end. The previous file is $1.bak-alice."
      fi
      cs_state="installed ($1)"
      return 0
      ;;
    11)
      say "Codex: $1 already has Alice's block of capture hooks; it was left as it is."
      cs_state="installed ($1)"
      return 0
      ;;
    14)
      say "Codex: $1 has hooks of its own and already has the Alice capture entries; it was left as it is."
      cs_state="entries present"
      return 0
      ;;
    12) ct_why="Alice could not add its hooks to it safely ($ct_how)" ;;
    *)
      # Nothing could check the file: entries already in it (a block added by hand, say) are
      # never printed again for a second copy.
      if grep -Eq "$cs_re" "$1" 2>/dev/null; then
        say "Codex: $1 has hooks of its own and already has the Alice capture entries; it was left as it is."
        cs_state="entries present"
        return 0
      fi
      if [ "$ct_rc" -eq 13 ]; then
        ct_why="there is no python3 with a TOML reader (Python 3.11 or later) to check the edit"
      else
        ct_why="python3 failed while checking the edit"
      fi
      ;;
  esac
  ct_block=$(sh "$DIR/alice-capture.sh" print-hooks-toml </dev/null 2>/dev/null)
  case $ct_block in
    "# >>> alice capture"*) ;;
    *)
      say "Codex: $1 has hooks of its own, and the hook script printed no block for it, so nothing was written for Codex; run the command again."
      cs_state="not written"
      return 0
      ;;
  esac
  say "Codex: $1 has hooks of its own (Codex reads one format per layer), and $ct_why, so it was left as it is. After a backup, add this block at its end:"
  say "$ct_block"
  cs_state="entries printed"
}

main() {
  [ -n "${HOME:-}" ] || die "HOME is not set"
  command -v curl >/dev/null 2>&1 || die "curl is not installed; install it, then run the command again"
  DIR="$HOME/.config/alice"
  MP="$DIR/claude-marketplace"
  TMPD=$(mktemp -d "${TMPDIR:-/tmp}/alice-setup.XXXXXX") || die "cannot make a temporary directory"
  chmod 700 "$TMPD"
  trap cleanup EXIT
  trap 'exit 129' HUP
  trap 'exit 130' INT
  trap 'exit 143' TERM

  # 1. The address: the file wins over the command line.
  arg=${1:-}
  arg=${arg%/}
  url_new=
  if [ -f "$DIR/url" ]; then
    url=$(head -n 1 "$DIR/url" | tr -d ' \t\r')
    url=${url%/}
    valid_url "$url" ||
      die "~/.config/alice/url names an address the token is never sent to; it must start with https:// (http:// only to 127.0.0.1 or localhost). Fix the file and run the command again"
    if [ -n "$arg" ] && [ "$arg" != "$url" ]; then
      say "Note: ~/.config/alice/url names $url, not $arg; keeping the file (edit it if the command's address is the right one)."
    fi
  else
    [ -n "$arg" ] ||
      die "give your Alice address: curl -fsSL <address>/api/v1/capture/setup.sh | sh -s -- <address>"
    valid_url "$arg" ||
      die "the address must start with https:// (http:// only to 127.0.0.1 or localhost, for tests): $arg"
    url=$arg
    url_new=1
  fi

  # 2. The token: the file when the server takes it; otherwise one question.
  token_new=
  if [ -f "$DIR/token" ]; then
    token=$(head -n 1 "$DIR/token" | tr -d ' \t\r')
    if ! valid_token "$token"; then
      say "~/.config/alice/token does not hold a token this script can use."
      token=
    else
      check_token
      case $CODE in
        ok) ;;
        refused)
          say "The server refused the token in ~/.config/alice/token."
          token=
          ;;
        *) die "$CHECK_ERR" ;;
      esac
    fi
  fi
  if [ -z "$token" ]; then
    ask_token
    if ! valid_token "$token"; then
      token=
      die "that does not look like an Alice token (letters, digits and . _ ~ + / = - only); nothing was changed"
    fi
    check_token
    case $CODE in
      ok) token_new=1 ;;
      refused)
        token=
        die "the server refused that token too; check it with your admin and run the command again. Nothing was changed."
        ;;
      *)
        token=
        die "$CHECK_ERR"
        ;;
    esac
  fi

  # 3. Only now is anything written.
  mkdir -p "$DIR/state" || die "cannot create $DIR"
  chmod 700 "$DIR" "$DIR/state"
  if [ -n "$url_new" ]; then write_file "$DIR/url" "$url"; fi
  if [ -n "$token_new" ]; then
    TOKEN_TMP="$DIR/.token.$$"
    { printf '%s\n' "$token" >"$TOKEN_TMP" && chmod 600 "$TOKEN_TMP" && mv -f "$TOKEN_TMP" "$DIR/token"; } ||
      die "cannot write ~/.config/alice/token"
    TOKEN_TMP=
  fi
  chmod 600 "$DIR/token" "$DIR/url"
  rm -f "$DIR/paused-until"

  # 4. Options (no secret): the person's briefing choice survives a re-run.
  briefing=on
  if grep -q '^[[:space:]]*briefing[[:space:]]*=[[:space:]]*off' "$DIR/capture.conf" 2>/dev/null; then
    briefing=off
  fi
  write_file "$DIR/capture.conf" "briefing=$briefing"

  # 5. The hook script, from the tenant (public; no token needed). It updates itself once a day
  #    from now on (spec section 5.3 step 8), and this install counts as today's check.
  HOOK_TMP="$DIR/.alice-capture.sh.$$"
  curl -q -fsS --connect-timeout 5 --max-time 30 -o "$HOOK_TMP" "$url/api/v1/capture/hook.sh" 2>"$TMPD/curl.err" ||
    die "could not download the hook script from $url: $(head -n 1 "$TMPD/curl.err" 2>/dev/null)"
  check_hook "$HOOK_TMP" ||
    die "what $url served as the hook script is not a whole Alice hook script ($HOOK_WHY); check the address"
  chmod 700 "$HOOK_TMP"
  mv -f "$HOOK_TMP" "$DIR/alice-capture.sh" || die "cannot install ~/.config/alice/alice-capture.sh"
  HOOK_TMP=
  write_file "$DIR/update-checked" "$(date +%s)"

  # 6. The local marketplace and its plugin, at the tenant's version. Its hooks.json is what the
  #    hook script prints.
  hooks_claude=$(hooks_json claude-code)
  case $hooks_claude in
    '{'*) ;;
    *) die "the hook script from $url printed no hooks.json for the plugin; check the address" ;;
  esac
  version=$(curl -q -fsS --connect-timeout 5 --max-time 30 "$url/api/v1/ping" 2>/dev/null |
    sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([0-9A-Za-z.+-]*\)".*/\1/p' | head -n 1)
  [ -n "$version" ] || version=0.0.0
  mkdir -p "$MP/.claude-plugin" "$MP/alice-capture/.claude-plugin" "$MP/alice-capture/hooks" ||
    die "cannot create $MP"
  write_file "$MP/.claude-plugin/marketplace.json" "$(marketplace_json)"
  write_file "$MP/alice-capture/.claude-plugin/plugin.json" "$(plugin_json)"
  write_file "$MP/alice-capture/hooks/hooks.json" "$hooks_claude"

  # 7. The plugin, through the claude command when it works.
  plugin=
  if command -v claude >/dev/null 2>&1; then
    if claude plugin marketplace add "$MP" </dev/null >"$TMPD/claude.out" 2>&1 ||
      claude plugin marketplace update alice-local </dev/null >>"$TMPD/claude.out" 2>&1; then
      if claude plugin install alice-capture@alice-local --scope user </dev/null >>"$TMPD/claude.out" 2>&1; then
        plugin=1
      fi
    fi
  fi
  if [ -n "$plugin" ]; then
    plugin_state="installed with claude plugin"
  else
    plugin_state="type the two /plugin commands above"
    say ""
    say "Finish in Claude Code by typing these two commands (if the plugin is already installed, nothing more is needed):"
    say "  /plugin marketplace add \"$MP\""
    say "  /plugin install alice-capture@alice-local"
  fi

  # 8. Codex: its own hooks.json.
  cs_state=
  codex_setup

  # 9. What was done.
  distill=on
  if grep -q '"distill_provider"[[:space:]]*:[[:space:]]*"off"' "$TMPD/status" 2>/dev/null; then
    distill=off
  fi
  enabled=on
  if grep -q '"enabled"[[:space:]]*:[[:space:]]*false' "$TMPD/status" 2>/dev/null; then
    enabled=off
  fi
  if [ -n "$token_new" ]; then token_state=saved; else token_state=reused; fi
  say ""
  say "Alice capture is set up for $url."
  say "  token: ~/.config/alice/token ($token_state)"
  say "  hook script: ~/.config/alice/alice-capture.sh"
  say "  plugin: alice-capture@alice-local ($plugin_state)"
  say "  briefing: $briefing (briefing=off in ~/.config/alice/capture.conf turns it off)"
  say "  capture on the server: $enabled"
  say "  distillation: $distill (the server's setting)"
  if [ -n "$cs_state" ]; then say "  codex hooks: $cs_state"; fi
  say "Check:  sh ~/.config/alice/alice-capture.sh status"
  say "Remove: sh ~/.config/alice/alice-capture.sh uninstall"
  if [ -n "$cs_state" ]; then
    say "Start a new Claude Code (or Codex) session for the hooks to load."
    say "Codex: the next time you open Codex it asks 'Hooks need review' $(printf '\342\200\224') choose Trust all and continue (or review the Alice entries with /hooks)."
  else
    say "Start a new Claude Code session for the hooks to load."
  fi
  return 0
}

main "$@"
