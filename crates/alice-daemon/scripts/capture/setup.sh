#!/bin/sh
# Alice: capture this machine's Claude Code (and Codex) sessions into your Alice server tenant
# (SP2 spec section 5.4). Run the one line your admin gave you:
#
#   curl -fsSL https://<host>/u/<name>/api/v1/capture/setup.sh | sh -s -- https://<host>/u/<name>
#
# It reuses ~/.config/alice/url and ~/.config/alice/token when they exist (the team's setup
# document writes them) and asks for the token only when the file is missing or the server
# refuses it. Then it installs the hook script (~/.config/alice/alice-capture.sh) and a small
# Claude Code plugin from a local marketplace (~/.config/alice/claude-marketplace). Running it
# again updates everything in place; `sh ~/.config/alice/alice-capture.sh uninstall` removes it.
# When `codex` is installed it also writes $CODEX_HOME/hooks.json (default ~/.codex/hooks.json)
# if that file does not exist, and otherwise prints the entries to add; Codex runs a hook only
# after the person trusts it in /hooks.
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

# The hooks file for agent $1 (claude-code: the plugin's hooks.json; codex: $CODEX_HOME/hooks.json).
hooks_json() {
  # `|| true`: a missing script (after uninstall, or deleted by hand) must never fail a hook;
  # dash exits 2 on a file it cannot open, and exit 2 from Stop blocks the agent.
  hj_cmd='sh \"$HOME/.config/alice/alice-capture.sh\" hook '$1' || true'
  printf '{\n'
  if [ "$1" = claude-code ]; then
    printf '  "description": "Alice capture: each event hands the new transcript bytes to a background upload; SessionStart prints the briefing.",\n'
  fi
  printf '  "hooks": {\n'
  hj_sep=
  for hj_event in SessionStart Stop SubagentStop PreCompact SessionEnd; do
    hj_timeout=10
    # Codex gives SessionEnd at most 3 s.
    if [ "$1" = codex ] && [ "$hj_event" = SessionEnd ]; then hj_timeout=3; fi
    printf '%s    "%s": [\n' "$hj_sep" "$hj_event"
    printf '      { "hooks": [ { "type": "command", "command": "%s", "timeout": %s } ] }\n' "$hj_cmd" "$hj_timeout"
    printf '    ]'
    hj_sep=',
'
  done
  printf '\n  }\n'
  printf '}\n'
}

# Codex (spec section 5.4 step 8). hooks.json is written only when it does not exist, or is
# still exactly the file this script wrote earlier (then it is updated in place); a file of the
# person's own, or inline [hooks] tables in config.toml, are never edited: the entries are
# printed instead (or, when they are in the file already, said to be there).
#
# The record of what was written ($DIR/codex-hooks.written) is staged first, so a record that
# cannot be written leaves hooks.json untouched, and is reported as that.
codex_setup() {
  command -v codex >/dev/null 2>&1 || return 0
  cs_home="${CODEX_HOME:-$HOME/.codex}"
  cs_file="$cs_home/hooks.json"
  cs_new=$(hooks_json codex)
  cs_sp='[[:space:]]'
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
  elif grep -Eq "^${cs_sp}*(\\[${cs_sp}*){0,2}[\"']?hooks[\"']?${cs_sp}*[].=]" "$cs_home/config.toml" 2>/dev/null; then
    cs_why="$cs_home/config.toml already has [hooks] tables, so hooks.json is not added beside them"
    if grep -Eq "$cs_re" "$cs_home/config.toml" 2>/dev/null; then cs_have=1; fi
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
  say "Codex runs a hook only after you trust it: open Codex, type /hooks, review the Alice capture hooks and trust them."
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

  # 5. The hook script, from the tenant (public; no token needed).
  HOOK_TMP="$DIR/.alice-capture.sh.$$"
  curl -q -fsS --connect-timeout 5 --max-time 30 -o "$HOOK_TMP" "$url/api/v1/capture/hook.sh" 2>"$TMPD/curl.err" ||
    die "could not download the hook script from $url: $(head -n 1 "$TMPD/curl.err" 2>/dev/null)"
  if ! head -n 1 "$HOOK_TMP" | grep -q '^#!/bin/sh' || ! sh -n "$HOOK_TMP" 2>/dev/null; then
    die "what $url served as the hook script is not a shell script; check the address"
  fi
  chmod 700 "$HOOK_TMP"
  mv -f "$HOOK_TMP" "$DIR/alice-capture.sh" || die "cannot install ~/.config/alice/alice-capture.sh"
  HOOK_TMP=

  # 6. The local marketplace and its plugin, at the tenant's version.
  version=$(curl -q -fsS --connect-timeout 5 --max-time 30 "$url/api/v1/ping" 2>/dev/null |
    sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([0-9A-Za-z.+-]*\)".*/\1/p' | head -n 1)
  [ -n "$version" ] || version=0.0.0
  mkdir -p "$MP/.claude-plugin" "$MP/alice-capture/.claude-plugin" "$MP/alice-capture/hooks" ||
    die "cannot create $MP"
  write_file "$MP/.claude-plugin/marketplace.json" "$(marketplace_json)"
  write_file "$MP/alice-capture/.claude-plugin/plugin.json" "$(plugin_json)"
  write_file "$MP/alice-capture/hooks/hooks.json" "$(hooks_json claude-code)"

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
  else
    say "Start a new Claude Code session for the hooks to load."
  fi
  return 0
}

main "$@"
