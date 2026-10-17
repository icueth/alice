#!/bin/sh
# alice-capture-hook: the Alice capture hook for server tenants (SP2 spec section 5.3, section 5.7).
# alice-capture-version: unstamped
#
# The tenant stamps the version line when it serves this script (a hash of its text). Once a day
# a background worker fetches the tenant's copy over https:// and, when its version differs and it
# passes the checks in verify_script, replaces this file with it (spec section 5.3 step 8).
#
# The contract across versions. After an update, a hook of the old version (a session started
# before it) starts the new file's `run`, and a worker of the new version meets what the old one
# left. So these stay compatible for at least one version: the `run` arguments, the state file's
# format, the lock layout (state/<key>.lock/ with pid and at, state/update.lock/), the
# update-checked record, and what `print-hooks-json` prints (the plugin's hooks.json and the Codex
# one are compared with it byte for byte).
#
# Test-only switches (environment): ALICE_CAPTURE_TEST_UPDATE_HTTP=1 lets self-update use a
# loopback http:// tenant; ALICE_CAPTURE_TEST_CLAUDE_LIMIT=<seconds> shortens the 120 s limit of
# the background `claude plugin` calls.
#
# Claude Code runs it through the alice-capture plugin on five events:
#   sh "$HOME/.config/alice/alice-capture.sh" hook claude-code
# Codex runs it from $CODEX_HOME/hooks.json (setup.sh writes it) on the same five events:
#   sh "$HOME/.config/alice/alice-capture.sh" hook codex
# It reads the hook's JSON on stdin, hands the work to a background copy of itself and returns
# at once. Only SessionStart waits, at most 2 s, for the briefing it prints.
#
# Subcommands:
#   hook <agent>       the hook itself (reads stdin); <agent> is claude-code or codex
#   run <agent> <event> <session> <path> <cwd> [<agent_id> <agent_path>]
#                      the background worker (no secret among its arguments); it also runs the
#                      daily update check
#   print-hooks-json <agent>
#                      the hooks file setup.sh writes for <agent> (the plugin's hooks.json, or
#                      $CODEX_HOME/hooks.json); the one place the hooks are written down
#   status             what is set up, and whether the server accepts the token
#   uninstall          removes the plugin and, after asking, the capture files
#
# Files, under ~/.config/alice (0700; every file 0600, this script 0700):
#   token, url         read only, never written here (the team's setup document or setup.sh)
#   capture.conf       briefing=on|off
#   state/<agent>-<session>[-<agent_id>].offset   (<agent> is claude-code or codex)
#                      "<offset> <path>", then session, subagent, cwd, seen (when the transcript
#                      was last there), and hold/stopped lines; its mtime is set back to 1971
#                      while work is left, so a sweep sees the transcript as newer
#   state/<key>.lock/  pid and at (refreshed every pass) of the worker sending that transcript
#   state/update.lock/ pid and at of the worker running the update check
#   capture.log        at most 256 KB; never the token, never transcript text
#   paused-until       when the server refused the token: no upload before this time
#   update-checked     when the last update check started (epoch seconds; setup writes it too)
#   .alice-capture.sh.new.<pid>   a downloaded copy while it is checked (removed after)
#
# The token leaves the shell only as a curl config line on curl's stdin (curl -q -K -), written
# by the printf builtin: never in argv, a URL, a child's environment, a log or an error.
# POSIX sh; needs curl, sed, head, tail, wc, od, ls, sort, cmp, cp, mkdir, mktemp, mv, find, touch
# and date; git, awk and dash are optional.
# Never run it with set -x: that would print the token.

set -u
umask 077
LC_ALL=C
export LC_ALL

DIR="${HOME:?}/.config/alice"
STATE="$DIR/state"
LOG="$DIR/capture.log"
CONF="$DIR/capture.conf"
PAUSED="$DIR/paused-until"
TOKEN_FILE="$DIR/token"
URL_FILE="$DIR/url"
SCRIPT="$DIR/alice-capture.sh"
MP="$DIR/claude-marketplace"
CHECKED="$DIR/update-checked"
UPDATE_EVERY=86400
SCRIPT_MAX=1048576
LOG_MAX=262144
BODY_START=2097152
BODY_MAX=8388608
PASSES=16
SWEEP_MAX=5
NL='
'
SOH=$(printf '\001')

token=
url=
hint=
ph_top=
tmpd=
lockdir=
upd_tmp=

cleanup() {
  unlock
  if [ -n "$tmpd" ]; then
    rm -rf "$tmpd"
    tmpd=
  fi
  if [ -n "$upd_tmp" ]; then
    rm -f "$upd_tmp"
    upd_tmp=
  fi
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

# The size of file $1 in bytes (0 when it cannot be read).
fsize() {
  fs_n=$({ wc -c <"$1"; } 2>/dev/null) || fs_n=0
  fs_n=${fs_n##* }
  case $fs_n in '' | *[!0-9]*) fs_n=0 ;; esac
  printf '%s' "$fs_n"
}

# Whether $1 is an epoch in seconds that is safe for arithmetic: 1 to 11 digits, no leading 0
# (dash aborts on a number it reads as octal, and on one too large).
epoch_ok() {
  case $1 in '' | 0* | *[!0-9]*) return 1 ;; esac
  [ ${#1} -le 11 ]
}

# Whether $1 is a byte offset that is safe for arithmetic: 0, or 1 to 16 digits with no leading 0
# (the server's offsets stop at 2^53).
offset_ok() {
  case $1 in 0) return 0 ;; '' | 0* | *[!0-9]*) return 1 ;; esac
  [ ${#1} -le 16 ]
}

# One line in capture.log, which is kept under 256 KB. Never the token or transcript text.
log() {
  [ -d "$DIR" ] || return 0
  if [ "$(fsize "$LOG")" -gt "$LOG_MAX" ]; then
    { tail -c $((LOG_MAX / 2)) "$LOG" | sed 1d >"$LOG.$$"; } 2>/dev/null && mv -f "$LOG.$$" "$LOG"
  fi
  printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >>"$LOG" 2>/dev/null
}

# A session or agent id as the server takes it: 1 to 128 of [A-Za-z0-9._-], starting alnum.
valid_id() {
  case $1 in '' | [!A-Za-z0-9]* | *[!A-Za-z0-9._-]*) return 1 ;; esac
  [ ${#1} -le 128 ]
}

# The Codex rollout of session $1 when the hook gave no transcript_path (it can be null): the
# newest seven $CODEX_HOME/sessions/YYYY/MM/DD folders are searched for rollout-*-<session>.jsonl
# (the session id is the end of the file name, and is matched whole). Prints the path (nothing
# when there is none). A compressed .jsonl.zst rollout is never matched.
find_rollout() {
  valid_id "$1" || return 1
  fr_root="${CODEX_HOME:-$HOME/.codex}/sessions"
  [ -d "$fr_root" ] || return 1
  ls -d "$fr_root"/[0-9][0-9][0-9][0-9]/[0-9][0-9]/[0-9][0-9] 2>/dev/null | sort -r | head -n 7 |
    while IFS= read -r fr_d; do
      for fr_f in "$fr_d"/rollout-*-"$1".jsonl; do
        if [ -f "$fr_f" ]; then
          printf '%s\n' "$fr_f"
          break 2
        fi
      done
    done | head -n 1
}

# The agent of state key $1 (claude-code-<session>... or codex-<session>).
key_agent() {
  case $1 in
    codex-*) printf codex ;;
    *) printf claude-code ;;
  esac
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

# Reads the token and the URL (head -n 1 of each); 1, logged, when either is missing or unusable.
load_credentials() {
  if [ ! -r "$TOKEN_FILE" ] || [ ! -r "$URL_FILE" ]; then
    log "no token or url file in ~/.config/alice; run the setup command"
    return 1
  fi
  token=$(head -n 1 "$TOKEN_FILE" 2>/dev/null | tr -d ' \t\r')
  url=$(head -n 1 "$URL_FILE" 2>/dev/null | tr -d ' \t\r')
  url=${url%/}
  if ! valid_token "$token"; then
    token=
    log "the token file holds no usable token; run the setup command again"
    return 1
  fi
  if ! valid_url "$url"; then
    log "the url file names an address the token is never sent to (it must be https://); run the setup command again"
    return 1
  fi
  return 0
}

# True while the server's refusal of the token holds capture back.
paused() {
  [ -f "$PAUSED" ] || return 1
  pu_until=$(head -n 1 "$PAUSED" 2>/dev/null)
  epoch_ok "$pu_until" || return 1
  [ "$(date +%s)" -lt "$pu_until" ]
}

briefing_on() {
  if grep -q '^[[:space:]]*briefing[[:space:]]*=[[:space:]]*off' "$CONF" 2>/dev/null; then
    return 1
  fi
  return 0
}

make_tmp() {
  [ -n "$tmpd" ] && return 0
  tmpd=$(mktemp -d "${TMPDIR:-/tmp}/alice-capture.XXXXXX" 2>/dev/null) || {
    tmpd=
    log "cannot make a temporary directory; event skipped"
    return 1
  }
  chmod 700 "$tmpd"
}

# The project hint of directory $1, as the MCP headersHelper names it: the basename of its git
# top-level when that is not $HOME, with " and \ removed; none outside git, or for a name with
# a control character. Sets $hint (percent-encoded, so any UTF-8 name travels as ASCII) and
# $ph_top (the top-level, for .alice.toml).
project_hint() {
  hint=
  ph_top=
  [ -n "${1:-}" ] && [ -d "$1" ] || return 0
  command -v git >/dev/null 2>&1 || return 0
  ph_top=$(git -C "$1" rev-parse --show-toplevel 2>/dev/null </dev/null && printf x) || ph_top=
  ph_top=${ph_top%x}
  ph_top=${ph_top%"$NL"}
  [ -n "$ph_top" ] || return 0
  ph_home=$(cd "$HOME" 2>/dev/null && pwd -P) || ph_home=
  if [ "$ph_top" = "$HOME" ] || [ "$ph_top" = "$ph_home" ]; then
    return 0
  fi
  ph_name=$(printf '%s' "${ph_top##*/}" | tr -d '"\\')
  case $ph_name in '' | *[[:cntrl:]]*) return 0 ;; esac
  hint=$(printf '%s' "$ph_name" | od -An -v -tx1 | tr -d ' \n' | sed 's/../%&/g')
}

# The curl config lines every request sends: the token, and the project hint when there is one.
send_headers() {
  printf 'header = "Authorization: Bearer %s"\n' "$token"
  if [ -n "$hint" ]; then
    printf 'header = "X-Alice-Project: %s"\n' "$hint"
  fi
}

# ---- The hook's input ----

# The still-escaped JSON string value of key $1 in $input (empty when absent or not a string).
json_raw() {
  printf '%s' "$input" | sed -n -E 's/.*"'"$1"'"[[:space:]]*:[[:space:]]*"(([^"\\]|\\.)*)".*/\1/p'
}

# Sets $val to the decoded value of key $1. Only \\, \" and \/ are undone: 1 when the value
# holds any other escape (a \u escape is never guessed) or a raw control character.
json_value() {
  val=$(json_raw "$1")
  case $val in *[[:cntrl:]]*) val= && return 1 ;; esac
  case $val in *\\*) ;; *) return 0 ;; esac
  val=$(printf '%s' "$val" | sed -e "s/\\\\\\\\/$SOH/g" -e 's/\\"/"/g' -e 's/\\\//\//g')
  case $val in *\\*) val= && return 1 ;; esac
  val=$(printf '%s' "$val" | sed "s/$SOH/\\\\/g")
  return 0
}

# The hook: read the input, start the background worker, print the briefing on SessionStart.
hook_main() {
  h_agent=${1:-}
  input=$(head -c 1048576 | tr '\r\n\t' '   ')
  case $h_agent in
    claude-code | codex) ;;
    *)
      log "an event for an unknown agent was ignored"
      return 0
      ;;
  esac
  if ! json_value session_id || ! valid_id "$val"; then
    log "a hook input without a usable session_id was ignored"
    return 0
  fi
  h_session=$val
  for h_field in hook_event_name transcript_path cwd agent_id agent_transcript_path source; do
    if ! json_value "$h_field"; then
      log "session $h_session: the hook input's $h_field holds a JSON escape this script does not decode (such as \\u); event skipped"
      return 0
    fi
    case $h_field in
      hook_event_name) h_name=$val ;;
      transcript_path) h_path=$val ;;
      cwd) h_cwd=$val ;;
      agent_id) h_sub=$val ;;
      agent_transcript_path) h_subpath=$val ;;
      source) h_source=$val ;;
    esac
  done
  case $h_name in
    SessionStart) h_event=session-start ;;
    Stop) h_event=stop ;;
    SubagentStop) h_event=subagent-stop ;;
    PreCompact) h_event=pre-compact ;;
    SessionEnd) h_event=session-end ;;
    *) return 0 ;;
  esac
  case $h_path in
    *.zst)
      log "session $h_session: the transcript is a compressed .zst file, which is not captured in this version; event skipped"
      return 0
      ;;
  esac
  case $h_path in
    /*) ;;
    *)
      if [ "$h_agent" = codex ]; then
        # Codex can send a null transcript_path: the background worker searches for the rollout.
        h_path=
      else
        log "session $h_session: the hook input names no transcript_path; event skipped"
        return 0
      fi
      ;;
  esac
  # A subagent's own transcript is its own source; anything less is the main session's.
  if [ "$h_event" != subagent-stop ] || ! valid_id "$h_sub" || [ "$h_agent" = codex ]; then
    h_sub=
  fi
  case $h_subpath in /*) ;; *) h_sub= ;; esac
  if [ -f "$0" ]; then
    if [ -n "$h_sub" ]; then
      nohup sh "$0" run "$h_agent" "$h_event" "$h_session" "$h_path" "$h_cwd" "$h_sub" "$h_subpath" \
        >/dev/null 2>&1 </dev/null &
    else
      nohup sh "$0" run "$h_agent" "$h_event" "$h_session" "$h_path" "$h_cwd" \
        >/dev/null 2>&1 </dev/null &
    fi
  fi
  # The briefing is Claude Code's: its SessionStart hook output becomes session context.
  if [ "$h_event" = session-start ] && [ "$h_agent" = claude-code ]; then
    brief "$h_source" "$h_cwd"
  fi
  return 0
}

# The SessionStart briefing (spec section 5.7): plain text on stdout, at most 2 s, nothing on failure.
brief() {
  case ${1:-} in
    resume | fork) return 0 ;;
    startup | clear | compact) b_query="?source=$1" ;;
    *) b_query= ;;
  esac
  briefing_on || return 0
  paused && return 0
  load_credentials || return 0
  project_hint "${2:-}"
  make_tmp || return 0
  b_got=$(send_headers | curl -q -K - -sS --connect-timeout 1 --max-time 2 -o "$tmpd/briefing" \
    -w '%{http_code} %{content_type}' "$url/api/v1/capture/briefing$b_query" 2>/dev/null) || b_got=
  case $b_got in
    '200 text/plain'*) cat "$tmpd/briefing" 2>/dev/null ;;
  esac
  return 0
}

# ---- The background worker ----

# Reads state file $1 into s_off s_path s_session s_sub s_cwd s_seen s_hold s_stopped (1 if
# absent). A corrupt offset counts as 0 (the server then says where to resume) and a corrupt hold
# as none; s_corrupt names what was dropped.
read_state() {
  s_off=0 s_path= s_session= s_sub= s_cwd= s_seen= s_hold= s_stopped= s_corrupt=
  [ -f "$1" ] || return 1
  rs_first=1
  while IFS= read -r rs_line || [ -n "$rs_line" ]; do
    if [ -n "$rs_first" ]; then
      rs_first=
      s_off=${rs_line%% *}
      s_path=${rs_line#* }
      continue
    fi
    case $rs_line in
      "session "*) s_session=${rs_line#session } ;;
      "subagent "*) s_sub=${rs_line#subagent } ;;
      "cwd "*) s_cwd=${rs_line#cwd } ;;
      "seen "*) s_seen=${rs_line#seen } ;;
      "hold "*) s_hold=${rs_line#hold } ;;
      "stopped "*) s_stopped=${rs_line#stopped } ;;
    esac
  done <"$1"
  if ! offset_ok "$s_off"; then s_off=0 s_corrupt="its offset"; fi
  if [ -n "$s_hold" ] && ! offset_ok "$s_hold"; then s_hold= s_corrupt="${s_corrupt:+$s_corrupt and }its hold"; fi
  epoch_ok "$s_seen" || s_seen=
  return 0
}

# Writes the s_* values to state file $1 (written whole, then moved into place). `seen` is
# now when the transcript exists (or for a new state), else it keeps its last value.
write_state() {
  if [ -f "$s_path" ] || [ -z "$s_seen" ]; then s_seen=$(date +%s); fi
  {
    printf '%s %s\n' "$s_off" "$s_path"
    printf 'session %s\n' "$s_session"
    if [ -n "$s_sub" ]; then printf 'subagent %s\n' "$s_sub"; fi
    printf 'cwd %s\n' "$s_cwd"
    printf 'seen %s\n' "$s_seen"
    if [ -n "$s_hold" ]; then printf 'hold %s\n' "$s_hold"; fi
    if [ -n "$s_stopped" ]; then printf 'stopped %s\n' "$s_stopped"; fi
  } >"$1.tmp.$$" 2>/dev/null && mv -f "$1.tmp.$$" "$1"
}

# Whether lock directory $1 may be broken (we tried $2 times). Its holder writes `pid`, then `at`,
# and refreshes `at` at the start of every pass; one pass (one upload) may take 5 + 300 s.
# - A live holder (kill -0 on its pid) is never broken until its `at` is 900 s old, a guard
#   against a pid reused by another process.
# - A dead or unknown holder is broken once its `at` is 360 s old.
# - Without a usable `at` (a holder killed between mkdir and writing it, or inside unlock, or a
#   full disk), the directory's own age stands in for it: more than 15 minutes for a live pid,
#   more than 6 otherwise. A dead or unknown holder is also broken after 5 tries.
lock_stale() {
  ls_at=$(cat "$1/at" 2>/dev/null)
  ls_pid=$(cat "$1/pid" 2>/dev/null)
  ls_alive=
  case $ls_pid in
    '' | *[!0-9]*) ;;
    *) if kill -0 "$ls_pid" 2>/dev/null; then ls_alive=1; fi ;;
  esac
  if ! epoch_ok "$ls_at"; then
    if [ -n "$ls_alive" ]; then
      [ -n "$(find "$1" -prune -mmin +15 2>/dev/null)" ]
    else
      [ "$2" -gt 5 ] || [ -n "$(find "$1" -prune -mmin +6 2>/dev/null)" ]
    fi
    return
  fi
  ls_age=$(($(date +%s) - ls_at))
  if [ -n "$ls_alive" ]; then
    [ "$ls_age" -gt 900 ]
  else
    [ "$ls_age" -gt 360 ]
  fi
}

# Writes the s_* values to state file $1 only while this worker holds the lock; 1, writing
# nothing, once another worker has taken it over.
save_state() {
  own_lock || return 1
  write_state "$1"
}

# Writes the line $2 to file $1 in a lock directory: a temporary file beside it, then mv. So a
# reader never sees `at` or `pid` empty (an empty `at` on an old directory would make a live lock
# look abandoned), and the directory's mtime moves with every write.
lock_put() {
  { printf '%s\n' "$2" >"$1.tmp.$$" && mv -f "$1.tmp.$$" "$1"; } 2>/dev/null || rm -f "$1.tmp.$$" 2>/dev/null
}

# Takes the lock of state key $1: a directory holding the owner's pid and `at`. A stale lock is
# moved aside with one atomic mv (only one breaker wins) and then removed. Waits up to $2
# seconds for a live holder.
lock() {
  lk_dir="$STATE/$1.lock"
  lk_tries=0
  while ! mkdir "$lk_dir" 2>/dev/null; do
    lk_tries=$((lk_tries + 1))
    if [ "$lk_tries" -le 60 ] && lock_stale "$lk_dir" "$lk_tries"; then
      rm -rf "$lk_dir.stale.$$"
      if mv "$lk_dir" "$lk_dir.stale.$$" 2>/dev/null; then
        rm -rf "$lk_dir.stale.$$"
        log "broke a stale lock of $1"
      fi
      continue
    fi
    [ "$lk_tries" -le "$2" ] || return 1
    sleep 1
  done
  lock_put "$lk_dir/pid" "$$"
  lock_put "$lk_dir/at" "$(date +%s)"
  lockdir=$lk_dir
  return 0
}

# Whether this process still holds the lock it took (another worker may have broken it).
own_lock() {
  [ -n "$lockdir" ] && [ "$(cat "$lockdir/pid" 2>/dev/null)" = "$$" ]
}

# Shows life at the start of every pass; 1 when the lock is no longer ours.
refresh_lock() {
  own_lock || return 1
  lock_put "$lockdir/at" "$(date +%s)"
  return 0
}

# Removes the lock only when this process owns it.
unlock() {
  if own_lock; then
    rm -f "$lockdir/at" "$lockdir/pid" "$lockdir"/*.tmp.* 2>/dev/null
    rmdir "$lockdir" 2>/dev/null
  fi
  lockdir=
}

# The byte offset just past the last complete line of file $1 (0 when it has none).
complete_end() {
  ce_size=$(fsize "$1")
  if [ "$ce_size" -eq 0 ]; then
    printf 0
    return 0
  fi
  ce_last=$({ tail -c 1 "$1" | od -An -tx1; } 2>/dev/null | tr -d ' \n')
  if [ "$ce_last" = 0a ]; then
    printf '%s' "$ce_size"
    return 0
  fi
  ce_tail=$({ tail -n 1 "$1" | wc -c; } 2>/dev/null)
  ce_tail=${ce_tail##* }
  case $ce_tail in '' | *[!0-9]*) ce_tail=$ce_size ;; esac
  printf '%s' $((ce_size - ce_tail))
}

# Sets state file $1's mtime back to 1971: work is left, so any transcript looks newer to a
# sweep. A successful write gives it the current time again.
mark_pending() {
  touch -t 197101010000 "$1" 2>/dev/null
}

# POSTs $tmpd/body to the transcript route with query $1; prints the HTTP status (000 when
# the server was not reached) and leaves the answer in $tmpd/answer.
post() {
  rm -f "$tmpd/answer"
  {
    send_headers
    printf 'header = "Content-Type: application/x-ndjson"\n'
    printf 'header = "Expect:"\n'
  } | curl -q -K - -sS --connect-timeout 5 --speed-limit 2048 --speed-time 20 --max-time 300 \
    -o "$tmpd/answer" -w '%{http_code}' \
    --data-binary "@$tmpd/body" "$url/api/v1/capture/transcript?$1" 2>/dev/null
}

# Sends what transcript $5 gained since its stored offset, in at most $PASSES uploads. Each
# upload holds at most 2 MiB, and a capped body says more=1 so the server leaves its last turn
# open (a cut never splits a turn). When an answer takes nothing from a capped body (one turn or
# one line longer than the cap), the same offset is sent again with twice the cap, up to 8 MiB,
# where the server closes a pass that makes no progress and calls a line without an end
# oversized.
#   capture_one <key> <event> <session> <agent_id or ""> <path> <cwd> <lock wait seconds>
capture_one() {
  co_key=$1
  co_event=$2
  co_st="$STATE/$co_key.offset"
  if ! lock "$co_key" "$7"; then
    if [ "$7" -gt 0 ]; then
      log "session $3: another capture of this transcript is still running; the next event sends the rest"
    fi
    return 0
  fi
  read_state "$co_st" || :
  s_session=$3 s_sub=$4 s_path=$5 s_cwd=$6
  if [ -n "$s_stopped" ]; then
    unlock
    return 0
  fi
  # The repository's .alice.toml, at the git top-level, else in cwd (plan decision P7).
  project_hint "$s_cwd"
  co_conf=${ph_top:-$s_cwd}
  co_flags=
  if [ -n "$co_conf" ] && [ -e "$co_conf/.alice.toml" ]; then
    co_toml="$co_conf/.alice.toml"
    grep -E -q '^[[:space:]]*no_capture[[:space:]]*=[[:space:]]*true' "$co_toml" 2>/dev/null
    case $? in
      0)
        # Never sent, now or later: the offset moves past every complete line (the server
        # records a forward gap when capture resumes).
        if [ -f "$s_path" ]; then
          co_end=$(complete_end "$s_path")
          if [ "$co_end" -gt "$s_off" ]; then s_off=$co_end s_hold=; fi
          save_state "$co_st"
        fi
        log "session $3: .alice.toml says no_capture = true; event skipped, and its turns so far are never sent"
        unlock
        return 0
        ;;
      1) ;;
      *)
        log "session $3: .alice.toml cannot be read; event skipped"
        unlock
        return 0
        ;;
    esac
    if grep -E -q '^[[:space:]]*cloud_denied[[:space:]]*=[[:space:]]*true' "$co_toml" 2>/dev/null; then
      co_flags="&no_distill=1"
    fi
  fi
  if [ ! -f "$co_st" ] && save_state "$co_st"; then
    mark_pending "$co_st"
  fi
  if [ ! -f "$s_path" ] || ! make_tmp; then
    unlock
    return 0
  fi
  # Said only here, where the uploads follow and the state is written again at the end: a
  # stopped session (or a skipped event) neither asks the server nor repairs the file, so it
  # would say it again on every event.
  if [ -n "$s_corrupt" ]; then
    log "session $3: the state file held a corrupt value ($s_corrupt); asking the server where to resume"
  fi
  # SessionEnd closes a trailing turn the server waited on, so it may send once past a hold.
  co_bypass=
  if [ "$co_event" = session-end ]; then co_bypass=1; fi
  co_pass=0
  co_cap=$BODY_START
  co_pending=
  co_lost=
  co_ended=
  co_empty_sent=
  while [ "$co_pass" -lt "$PASSES" ]; do
    co_pass=$((co_pass + 1))
    co_empty=
    if ! refresh_lock; then
      co_lost=1
      break
    fi
    co_size=$(fsize "$s_path")
    if [ "$s_off" -gt "$co_size" ]; then
      log "session $3: the transcript is shorter than the stored offset; asking the server where to resume"
      s_off=0 s_hold=
    fi
    if [ "$s_off" -ge "$co_size" ]; then
      # SessionEnd with nothing left to send (a Stop sent the rest, or a 409 moved the offset to
      # the end of the file): one empty upload at the offset, so the server still ends the
      # session and marks it for distillation. Owed until the server accepts a session end in
      # this call; sent at most once.
      if [ "$co_event" = session-end ] && [ -z "$co_ended" ] && [ -z "$co_empty_sent" ] &&
        [ "$s_off" -gt 0 ]; then
        co_empty=1 co_empty_sent=1
      else
        break
      fi
    fi
    if [ -n "$s_hold" ] && [ "$co_size" -le "$s_hold" ] && [ -z "$co_bypass" ]; then
      break
    fi
    co_bypass=
    if [ -n "$co_empty" ]; then
      { : >"$tmpd/body"; } 2>/dev/null
    else
      { tail -c +$((s_off + 1)) "$s_path" | head -c "$co_cap" >"$tmpd/body"; } 2>/dev/null
    fi
    co_n=$(fsize "$tmpd/body")
    [ "$co_n" -gt 0 ] || [ -n "$co_empty" ] || break
    co_q="agent=$(key_agent "$co_key")&session=$3&offset=$s_off&event=$co_event$co_flags"
    # A capped body (as long as the cap): the transcript may go on, so the server keeps the
    # body's last turn open.
    if [ "$co_n" -ge "$co_cap" ]; then co_q="$co_q&more=1"; fi
    if [ -n "$4" ]; then co_q="$co_q&subagent=$4"; fi
    co_code=$(post "$co_q")
    co_rc=$?
    co_next=$(sed -n 's/.*"next_offset"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$tmpd/answer" 2>/dev/null | head -n 1)
    case $co_code in
      200)
        # The answer says whether this upload ended the session (a session end that landed).
        co_end_note=
        if grep -q '"ended"[[:space:]]*:[[:space:]]*true' "$tmpd/answer" 2>/dev/null; then
          co_ended=1 co_end_note="; the server ended the session"
        fi
        if [ -n "$co_empty" ]; then
          if [ -n "$co_end_note" ]; then
            log "session $3: nothing new at the session end; the server ended the session at offset $s_off"
          else
            co_stored=$(sed -n 's/.*"turns_stored"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$tmpd/answer" 2>/dev/null | head -n 1)
            log "session $3: the empty session end at offset $s_off came back with next_offset ${co_next:-none} and ${co_stored:-no} turns stored, and no end; the server did not end the session"
          fi
          break
        fi
        if grep -q '"oversized_line"[[:space:]]*:[[:space:]]*true' "$tmpd/answer" 2>/dev/null; then
          # One line of more than 8 MiB: measure it with awk and skip it (the server counts a gap).
          if ! command -v awk >/dev/null 2>&1; then
            log "session $3: an oversized line needs awk to be skipped, and awk is not installed"
            s_hold=$co_size
            break
          fi
          co_len=$({ tail -c +$((s_off + 1)) "$s_path" | awk '{ print length($0) + 1; exit }'; } 2>/dev/null)
          case $co_len in '' | *[!0-9]*) co_len=0 ;; esac
          if [ "$co_len" -lt 2 ] || [ $((s_off + co_len)) -gt "$co_size" ]; then
            s_hold=$co_size # the line has no end yet
            break
          fi
          co_nl=$({ tail -c +$((s_off + co_len)) "$s_path" | head -c 1 | od -An -tx1; } 2>/dev/null | tr -d ' \n')
          if [ "$co_nl" != 0a ]; then
            log "session $3: an oversized line at offset $s_off could not be measured; it is tried again when the transcript grows"
            s_hold=$co_size
            break
          fi
          log "session $3: skipped an oversized line of $co_len bytes at offset $s_off"
          s_off=$((s_off + co_len)) s_hold= co_cap=$BODY_START
          save_state "$co_st" || {
            co_lost=1
            break
          }
          continue
        fi
        case $co_next in
          '' | *[!0-9]*)
            log "session $3: the server's answer held no next_offset"
            break
            ;;
        esac
        if [ "$co_next" -eq "$s_off" ] && [ "$co_n" -ge "$co_cap" ] && [ "$co_cap" -lt "$BODY_MAX" ]; then
          # A capped body held no whole turn (or no line end): twice the cap, same offset.
          co_cap=$((co_cap * 2))
          if [ "$co_cap" -gt "$BODY_MAX" ]; then co_cap=$BODY_MAX; fi
          log "session $3: nothing complete in $co_n bytes from offset $s_off; sending up to $co_cap bytes"
          continue
        fi
        if [ "$co_next" -eq "$s_off" ]; then
          # Nothing taken in (rules pending, a subagent too short, no complete turn): not sent
          # again until the transcript grows past this size.
          log "session $3: the server took nothing from offset $s_off yet ($co_event); it is sent again when the transcript grows"
          s_hold=$co_size
          break
        fi
        if [ "$co_next" -lt "$s_off" ] || [ "$co_next" -gt $((s_off + co_n)) ]; then
          log "session $3: the server answered an offset outside the upload ($co_next); stopping here"
          break
        fi
        log "session $3: sent $co_n bytes from offset $s_off ($co_event); the server's cursor is at $co_next$co_end_note"
        s_off=$co_next s_hold= co_cap=$BODY_START
        save_state "$co_st" || {
          co_lost=1
          break
        }
        ;;
      409)
        case $co_next in
          '' | *[!0-9]*)
            log "session $3: a 409 answer held no next_offset"
            break
            ;;
        esac
        co_size=$(fsize "$s_path")
        if [ "$co_next" -gt "$co_size" ]; then
          log "session $3: the server's cursor is past this file's end; capture stopped for this session"
          s_stopped=$co_next
          break
        fi
        s_off=$co_next s_hold=
        save_state "$co_st" || {
          co_lost=1
          break
        }
        ;;
      401)
        log "the token was refused; run the setup command again (capture is paused for an hour)"
        { printf '%s\n' $(($(date +%s) + 3600)) >"$PAUSED.$$"; } 2>/dev/null && mv -f "$PAUSED.$$" "$PAUSED"
        co_pending=1
        break
        ;;
      400 | 413)
        co_kind=$(sed -n 's/.*"kind"[[:space:]]*:[[:space:]]*"\([a-z_]*\)".*/\1/p' "$tmpd/answer" 2>/dev/null | head -n 1)
        log "session $3: the server refused the upload (HTTP $co_code ${co_kind:-}); it is sent again when the transcript grows"
        s_hold=$co_size
        break
        ;;
      000 | '')
        log "session $3: could not reach the server (curl exit $co_rc); the next event retries"
        co_pending=1
        break
        ;;
      *)
        log "session $3: the server answered HTTP $co_code; the next event retries"
        co_pending=1
        break
        ;;
    esac
  done
  if [ -z "$co_lost" ] && ! own_lock; then co_lost=1; fi
  if [ -n "$co_lost" ]; then
    # The state and the lock are the new holder's now: write nothing, remove nothing.
    log "session $3: another worker took over this transcript's lock; stopping without writing"
    lockdir=
    return 0
  fi
  if [ -z "$co_pending" ] && [ -z "$s_stopped" ] && [ "$s_off" -lt "$(fsize "$s_path")" ] &&
    { [ -z "$s_hold" ] || [ "$(fsize "$s_path")" -gt "$s_hold" ]; }; then
    co_pending=1 # out of passes with bytes left
  fi
  if save_state "$co_st" && [ -n "$co_pending" ]; then mark_pending "$co_st"; fi
  unlock
  return 0
}

# SessionStart: finish up to five other transcripts that grew while nothing sent them.
sweep() {
  sw_n=0
  for sw_st in "$STATE"/*.offset; do
    [ -f "$sw_st" ] || continue
    paused && break
    sw_key=${sw_st##*/}
    sw_key=${sw_key%.offset}
    [ "$sw_key" != "$1" ] || continue
    read_state "$sw_st" || continue
    [ -z "$s_stopped" ] || continue
    valid_id "$s_session" || continue
    if [ -n "$s_sub" ]; then valid_id "$s_sub" || continue; fi
    # A Codex state is only ever made for a rollout that was found (run_main returns before
    # capture_one when the search finds nothing), so a vanished file is not searched for again:
    # another file would be paired with this file's stored offset.
    if [ ! -f "$s_path" ]; then
      # No transcript (gone, or not written yet): forget it a week after it was last there.
      # The `seen` line decides, not the mtime (which mark_pending sets back).
      if [ -n "$s_seen" ] && [ $(($(date +%s) - s_seen)) -gt 604800 ]; then rm -f "$sw_st"; fi
      continue
    fi
    # Untouched since its state was written (and no work left, see mark_pending): skip.
    [ "$s_path" -nt "$sw_st" ] || continue
    sw_size=$(fsize "$s_path")
    [ "$sw_size" -gt "$s_off" ] || continue
    if [ -n "$s_hold" ] && [ "$sw_size" -le "$s_hold" ]; then continue; fi
    capture_one "$sw_key" sweep "$s_session" "$s_sub" "$s_path" "$s_cwd" 0
    sw_n=$((sw_n + 1))
    [ "$sw_n" -lt "$SWEEP_MAX" ] || break
  done
}

run_main() {
  [ $# -ge 5 ] || return 0
  r_event=$2 r_session=$3 r_path=$4 r_cwd=$5 r_sub=${6:-} r_subpath=${7:-}
  r_agent=$1
  case $r_agent in
    claude-code | codex) ;;
    *) return 0 ;;
  esac
  case $r_event in session-start | stop | subagent-stop | pre-compact | session-end) ;; *) return 0 ;; esac
  valid_id "$r_session" || return 0
  if [ -n "$r_sub" ]; then valid_id "$r_sub" || return 0; fi
  [ -d "$DIR" ] || return 0
  paused && return 0
  load_credentials || return 0
  mkdir -p "$STATE" 2>/dev/null || return 0
  # Before the capture work, so a release whose capture path fails can still fetch its fix.
  self_update
  # The transcript may lag the event that names it.
  if [ "$r_event" != session-start ]; then sleep 2; fi
  if [ "$r_agent" = codex ] && [ -z "$r_path" ]; then
    r_path=$(find_rollout "$r_session")
    if [ -z "$r_path" ]; then
      log "session $r_session: no rollout file for this session in the newest seven day folders under \$CODEX_HOME/sessions; the next event looks again"
      return 0
    fi
  fi
  if [ -n "$r_sub" ]; then
    capture_one "$r_agent-$r_session-$r_sub" "$r_event" "$r_session" "$r_sub" "$r_subpath" "$r_cwd" 40
  else
    capture_one "$r_agent-$r_session" "$r_event" "$r_session" "" "$r_path" "$r_cwd" 40
  fi
  if [ "$r_event" = session-start ]; then
    sweep "$r_agent-$r_session"
  fi
  return 0
}

# ---- Self-update (spec section 5.3 step 8) ----

# The hooks file for agent $1: claude-code, the plugin's hooks.json; codex, $CODEX_HOME/hooks.json.
# setup.sh writes what this prints, and the update check compares with it, so the installed script
# is the one place the hooks are written down.
print_hooks_json() {
  case ${1:-} in claude-code | codex) ;; *) return 2 ;; esac
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

# The version on the version line of script file $1 (empty when it has none).
script_version() {
  head -n 5 "$1" 2>/dev/null |
    sed -n 's/^# alice-capture-version: \([A-Za-z0-9._+-]\{1,64\}\)$/\1/p' | head -n 1
}

# Whether the daily update check is due: no usable record of the last one (missing, or garbled:
# a leading 0, more than 11 digits), a record a day old, or one in the future (the clock was set
# back since).
update_due() {
  ud_at=$(head -n 1 "$CHECKED" 2>/dev/null)
  epoch_ok "$ud_at" || return 0
  ud_now=$(date +%s)
  [ "$ud_at" -gt "$ud_now" ] || [ $((ud_now - ud_at)) -ge "$UPDATE_EVERY" ]
}

# Whether downloaded file $1 may replace the installed script. 1, with the reason in vs_why, when
# it is empty, does not start with #!/bin/sh, lacks the marker or the version line, is cut short
# (its last line is not `main "$@"`), fails sh -n (or dash -n where dash exists), or fails its
# smoke run: `sh $1 print-hooks-json claude-code`, with the shell the hook uses, must exit 0
# within 20 s and print JSON (a script that dies as it starts would stop capture and its own
# updates for good).
verify_script() {
  vs_why=
  if [ ! -s "$1" ]; then
    vs_why="the download was empty"
  elif [ "$(head -n 1 "$1")" != '#!/bin/sh' ]; then
    vs_why="the download does not start with #!/bin/sh"
  elif ! sed -n 2p "$1" | grep -q '^# alice-capture-hook'; then
    vs_why="the download has no alice-capture-hook marker line"
  elif [ -z "$(script_version "$1")" ]; then
    vs_why="the download has no version line"
  elif [ "$(tail -n 1 "$1")" != 'main "$@"' ]; then
    vs_why="the download is cut short (it does not end with main)"
  elif ! sh -n "$1" >/dev/null 2>&1; then
    vs_why="the download fails sh -n"
  elif command -v dash >/dev/null 2>&1 && ! dash -n "$1" >/dev/null 2>&1; then
    vs_why="the download fails dash -n"
  elif ! bounded 20 sh "$1" print-hooks-json claude-code >"$tmpd/smoke" 2>/dev/null ||
    [ "$(head -c 1 "$tmpd/smoke")" != '{' ]; then
    vs_why="the download fails its smoke run (print-hooks-json claude-code)"
  fi
  [ -z "$vs_why" ]
}

# Runs command "$2" ... in the background with stdin from /dev/null, and ends it after $1 seconds
# (macOS has no timeout command): TERM, then KILL 2 s later when TERM did not end it (a script's
# own TERM trap waits for its foreground command). Returns the command's status, which is not 0
# once it was ended. Its stdout and stderr are bounded's own. A child the command started may
# outlive a KILL.
#
# The watchdog is a subshell that sleeps one second at a time and writes each sleep's pid into a
# tick file in this worker's temporary directory (cleanup removes it with the directory, so a
# caller that a TERM ends together with its watchdog leaves nothing in $TMPDIR). The caller, right
# after it has reaped the command, stops the watchdog with KILL (a trapped TERM can be lost), then
# kills the sleep the tick file names. That KILL is the only guard against signalling a reaped
# pid that was reused: the watchdog signals the command while `kill -0` finds it, so only the
# instant between the reap and the KILL is open. A sleep the watchdog forked but had not recorded
# yet when it was killed outlives the call by under a second. When the caller is killed, the
# watchdog still ends the command at the limit, then exits.
#
# With $tmpd empty there is no tick file, and the watchdog's last sleep may outlive each call by
# under a second. A caller that may outlive the worker's directory clears $tmpd first (the
# background `claude plugin` calls of sync_plugin): a tick file made while cleanup runs `rm -rf`
# on that directory would leave it behind.
bounded() {
  bd_limit=$1
  shift
  bd_tick=
  if [ -n "$tmpd" ] && { : >"$tmpd/bounded"; } 2>/dev/null; then bd_tick="$tmpd/bounded"; fi
  "$@" </dev/null &
  bd_pid=$!
  (
    trap - EXIT HUP INT TERM
    bd_n=0
    while kill -0 "$bd_pid" 2>/dev/null; do
      if [ "$bd_n" -eq "$bd_limit" ]; then
        kill -TERM "$bd_pid" 2>/dev/null
      elif [ "$bd_n" -ge $((bd_limit + 2)) ]; then
        kill -KILL "$bd_pid" 2>/dev/null
        break
      fi
      sleep 1 &
      # Only into the file the caller made: once cleanup has removed the directory, nothing is
      # written back into it.
      if [ -n "$bd_tick" ] && [ -f "$bd_tick" ]; then printf '%s\n' "$!" >"$bd_tick"; fi
      wait "$!"
      bd_n=$((bd_n + 1))
    done
  ) </dev/null >/dev/null 2>&1 &
  bd_dog=$!
  wait "$bd_pid"
  bd_rc=$?
  kill -KILL "$bd_dog" 2>/dev/null
  wait "$bd_dog" 2>/dev/null
  if [ -n "$bd_tick" ]; then
    bd_sleep=$(cat "$bd_tick" 2>/dev/null)
    case $bd_sleep in '' | *[!0-9]*) ;; *) kill -KILL "$bd_sleep" 2>/dev/null ;; esac
    rm -f "$bd_tick"
  fi
  return "$bd_rc"
}

# Temporary files of an update (or a setup) that was killed: removed under the update lock once
# they are more than 10 minutes old, so a run going on now keeps its own.
clean_stale_temps() {
  for ct_f in "$DIR"/.alice-capture.sh.new.* "$CHECKED".* "$MP"/alice-capture/hooks/hooks.json.tmp.* \
    "$MP"/alice-capture/.claude-plugin/plugin.json.tmp.* "$MP"/.claude-plugin/marketplace.json.tmp.* \
    "${CODEX_HOME:-$HOME/.codex}"/hooks.json.tmp.[0-9]* "$DIR"/codex-hooks.written.new; do
    [ -f "$ct_f" ] || continue
    if [ -n "$(find "$ct_f" -prune -mmin +10 2>/dev/null)" ]; then rm -f "$ct_f"; fi
  done
}

# Rewrites file $1 with the bytes of file $2 (a temporary file beside it, then mv); 1, leaving $1
# as it was, on failure.
replace_file() {
  if cp "$2" "$1.tmp.$$" 2>/dev/null && mv -f "$1.tmp.$$" "$1" 2>/dev/null; then
    return 0
  fi
  rm -f "$1.tmp.$$" 2>/dev/null
  return 1
}

# Sets the "version" of the plugin's JSON file $1 to $2 (a temporary file beside it, then mv).
set_json_version() {
  if sed 's/"version"[[:space:]]*:[[:space:]]*"[^"]*"/"version": "'"$2"'"/' "$1" >"$1.tmp.$$" 2>/dev/null &&
    mv -f "$1.tmp.$$" "$1" 2>/dev/null; then
    return 0
  fi
  rm -f "$1.tmp.$$" 2>/dev/null
  return 1
}

# Writes to file $2 the hooks file the installed script prints for agent $1; 1, logged, when it
# prints none.
printed_hooks() {
  if sh "$SCRIPT" print-hooks-json "$1" </dev/null >"$2" 2>/dev/null && [ "$(head -c 1 "$2")" = '{' ]; then
    return 0
  fi
  log "the installed hook script printed no $1 hooks file; the hooks were left as they are"
  return 1
}

# The plugin's hooks.json ($2) differs from $1, what the installed script prints. The plugin's
# version moves first, to <its version>+<the script's version> (a Claude Code that runs plugins
# from its cache copies a new version only), then hooks.json is rewritten, and `claude plugin` is
# asked to reload it in the background (stdin /dev/null). Claude Code reads a local plugin's own
# folder at the next session start anyway; when `claude` is missing or fails, one line says to run
# the setup command again.
sync_plugin() {
  sp_json="$MP/alice-capture/.claude-plugin/plugin.json"
  sp_v=$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([0-9A-Za-z.+-]*\)".*/\1/p' "$sp_json" 2>/dev/null | head -n 1)
  sp_v=${sp_v%%+*}
  sp_v="${sp_v:-0.0.0}+$(script_version "$SCRIPT")"
  case $sp_v in *+ | *[!0-9A-Za-z.+-]*) sp_v= ;; esac
  if [ -z "$sp_v" ] || ! set_json_version "$sp_json" "$sp_v" ||
    ! set_json_version "$MP/.claude-plugin/marketplace.json" "$sp_v" || ! replace_file "$2" "$1"; then
    log "the plugin's hooks.json could not be brought up to date with the hook script; run the setup command again"
    return 0
  fi
  log "the plugin's hooks.json changed with the hook script (plugin version $sp_v)"
  if ! command -v claude >/dev/null 2>&1; then
    log "the plugin's hooks.json changed, and there is no claude command to reload it with; run the setup command again (a new Claude Code session may load it anyway)"
    return 0
  fi
  # From $HOME (a project's directory could change what `claude plugin` reads), without Claude
  # Code's own self-update, and each call killed after 120 s. The subshell outlives the worker,
  # whose cleanup removes $tmpd meanwhile: it uses no temporary directory (bounded then keeps no
  # tick file), so it never writes into one being removed.
  (
    trap - EXIT HUP INT TERM
    tmpd=
    cd "$HOME" 2>/dev/null || exit 0
    sp_limit=${ALICE_CAPTURE_TEST_CLAUDE_LIMIT:-120}
    case $sp_limit in '' | 0* | *[!0-9]* | ?????*) sp_limit=120 ;; esac
    if bounded "$sp_limit" env DISABLE_AUTOUPDATER=1 claude plugin marketplace update alice-local >/dev/null 2>&1 &&
      bounded "$sp_limit" env DISABLE_AUTOUPDATER=1 claude plugin update alice-capture@alice-local >/dev/null 2>&1; then
      log "claude plugin took the new hooks.json; new Claude Code sessions load it"
    else
      log "the plugin's hooks.json changed, and claude plugin update failed or did not finish within $sp_limit s; run the setup command again so Claude Code loads the new hooks"
    fi
  ) </dev/null >/dev/null 2>&1 &
}

# The Codex hooks.json ($2) is still the file setup wrote and differs from $1, what the installed
# script prints: it is rewritten with its record, which is staged first, as setup does. Codex runs
# changed hooks only after the person trusts them again.
sync_codex() {
  sc_rec="$DIR/codex-hooks.written"
  if ! cp "$1" "$sc_rec.new" 2>/dev/null; then
    rm -f "$sc_rec.new" 2>/dev/null
    log "the Codex hooks were not updated: the record ~/.config/alice/codex-hooks.written could not be written; run the setup command again"
    return 0
  fi
  if ! replace_file "$2" "$1"; then
    rm -f "$sc_rec.new" 2>/dev/null
    log "the Codex hooks were not updated: $2 could not be written; run the setup command again"
    return 0
  fi
  if ! mv -f "$sc_rec.new" "$sc_rec" 2>/dev/null; then
    rm -f "$sc_rec.new" 2>/dev/null
    replace_file "$2" "$sc_rec" # back to the file the record names, so it stays setup's own
    log "the Codex hooks were not updated: the record ~/.config/alice/codex-hooks.written could not be written; run the setup command again"
    return 0
  fi
  log "updated $2 with the hook script; Codex runs the changed hooks only after you trust them again: open Codex, type /hooks and trust the Alice capture hooks"
}

# Brings the plugin's hooks.json, and the Codex hooks.json while it is still the file setup wrote,
# to what the installed script prints. Each is rewritten only when it differs.
sync_hooks() {
  [ -f "$SCRIPT" ] && make_tmp || return 0
  sh_hooks="$MP/alice-capture/hooks/hooks.json"
  if [ -f "$sh_hooks" ] && printed_hooks claude-code "$tmpd/hooks-claude" &&
    ! cmp -s "$tmpd/hooks-claude" "$sh_hooks"; then
    sync_plugin "$tmpd/hooks-claude" "$sh_hooks"
  fi
  sh_codex="${CODEX_HOME:-$HOME/.codex}/hooks.json"
  if [ -f "$DIR/codex-hooks.written" ] && [ -f "$sh_codex" ] && cmp -s "$sh_codex" "$DIR/codex-hooks.written" &&
    printed_hooks codex "$tmpd/hooks-codex" && ! cmp -s "$tmpd/hooks-codex" "$sh_codex"; then
    sync_codex "$tmpd/hooks-codex" "$sh_codex"
  fi
}

# The daily update check, at the start of a background worker (never the hook itself, and before
# the capture work), under the update lock. It fetches the tenant's hook script over https:// (a
# public route: no token is sent) and, when its version differs from the installed one and it
# passes verify_script, replaces the installed script: a temporary file beside it, 0700, then mv
# (a running worker keeps reading the old file). Then the hooks files follow the installed
# script. Any failure keeps the installed script byte for byte and logs one line, which never
# holds the token or the script's text.
self_update() {
  [ -f "$SCRIPT" ] || return 0
  update_due || return 0
  lock update 0 || return 0
  # Read again under the lock: another worker may have checked meanwhile.
  if ! update_due; then
    unlock
    return 0
  fi
  # The record first: no fetch without it, or every event would fetch.
  su_now=$(date +%s)
  { printf '%s\n' "$su_now" >"$CHECKED.$$" && mv -f "$CHECKED.$$" "$CHECKED"; } 2>/dev/null
  rm -f "$CHECKED.$$" 2>/dev/null
  if [ "$(head -n 1 "$CHECKED" 2>/dev/null)" != "$su_now" ]; then
    log "capture hook update check skipped: ~/.config/alice/update-checked cannot be written, and without it every event would check"
    unlock
    return 0
  fi
  clean_stale_temps
  case $url in
    https://*) ;;
    *)
      if [ "${ALICE_CAPTURE_TEST_UPDATE_HTTP:-}" != 1 ]; then
        log "capture hook update skipped: it runs over https:// only, and ~/.config/alice/url is not https:// (asked again in a day)"
        unlock
        return 0
      fi
      ;;
  esac
  if ! make_tmp; then
    unlock
    return 0
  fi
  su_old=$(script_version "$SCRIPT")
  upd_tmp="$DIR/.alice-capture.sh.new.$$"
  rm -f "$upd_tmp"
  su_code=$(curl -q -fsS --connect-timeout 5 --max-time 30 --max-filesize "$SCRIPT_MAX" -o "$upd_tmp" \
    -w '%{http_code}' "$url/api/v1/capture/hook.sh" 2>/dev/null)
  su_rc=$?
  if [ "$su_rc" -ne 0 ]; then
    log "capture hook update check failed (curl exit $su_rc, HTTP ${su_code:-000}); the installed script stays"
  elif [ "$su_code" != 200 ]; then
    log "capture hook update refused (HTTP $su_code, not 200); the installed script stays"
  elif ! verify_script "$upd_tmp"; then
    log "capture hook update refused (HTTP $su_code): $vs_why; the installed script stays"
  else
    su_new=$(script_version "$upd_tmp")
    if [ "$su_new" = "$su_old" ]; then
      :
    elif [ ! -f "$SCRIPT" ]; then
      log "capture hook update skipped: the hook script was removed meanwhile, and is not put back"
    elif chmod 700 "$upd_tmp" 2>/dev/null && mv -f "$upd_tmp" "$SCRIPT" 2>/dev/null; then
      upd_tmp=
      log "capture hook updated ${su_old:-unversioned} $(printf '\342\206\222') $su_new"
    else
      log "capture hook update could not replace ~/.config/alice/alice-capture.sh; the installed script stays"
    fi
  fi
  if [ -n "$upd_tmp" ]; then
    rm -f "$upd_tmp"
    upd_tmp=
  fi
  sync_hooks
  unlock
  return 0
}

# ---- status and uninstall ----

# Where the Codex hooks stand: in $CODEX_HOME/hooks.json (as setup wrote it, or changed), in an
# inline [hooks] table of config.toml, or not found. Silent when there is no Codex here.
codex_status() {
  cst_home="${CODEX_HOME:-$HOME/.codex}"
  cst_re='alice-capture\.sh.{1,3}hook codex'
  if [ -f "$cst_home/hooks.json" ] && grep -Eq "$cst_re" "$cst_home/hooks.json" 2>/dev/null; then
    if [ -f "$DIR/codex-hooks.written" ] && cmp -s "$cst_home/hooks.json" "$DIR/codex-hooks.written"; then
      printf 'codex hooks: installed (%s/hooks.json); Codex runs them only after you trust them in /hooks\n' "$cst_home"
    else
      printf 'codex hooks: in %s/hooks.json (changed since setup wrote it, or added by hand); Codex runs them only after you trust them in /hooks\n' "$cst_home"
    fi
  elif grep -Eq "$cst_re" "$cst_home/config.toml" 2>/dev/null; then
    printf 'codex hooks: in %s/config.toml; Codex runs them only after you trust them in /hooks\n' "$cst_home"
  elif command -v codex >/dev/null 2>&1 || [ -f "$DIR/codex-hooks.written" ]; then
    printf 'codex hooks: not found; run the setup command\n'
  fi
}

status_main() {
  printf 'Alice capture for a server tenant\n'
  if [ -r "$URL_FILE" ]; then
    printf 'url: %s\n' "$(head -n 1 "$URL_FILE" | tr -d ' \t\r')"
  else
    printf 'url: missing (~/.config/alice/url)\n'
  fi
  if [ -r "$TOKEN_FILE" ]; then
    printf 'token: ~/.config/alice/token\n'
  else
    printf 'token: missing (~/.config/alice/token)\n'
  fi
  if load_credentials && make_tmp; then
    st_code=$(printf 'header = "Authorization: Bearer %s"\n' "$token" | curl -q -K - -sS --connect-timeout 5 --max-time 30 \
      -o "$tmpd/status" -w '%{http_code}' "$url/api/v1/capture/status" 2>/dev/null)
    case $st_code in
      200)
        printf 'server: reachable, token accepted\n'
        if grep -q '"enabled"[[:space:]]*:[[:space:]]*false' "$tmpd/status"; then
          printf 'capture on the server: off\n'
        else
          printf 'capture on the server: on\n'
        fi
        if grep -q '"distill_provider"[[:space:]]*:[[:space:]]*"off"' "$tmpd/status"; then
          printf 'distillation: off\n'
        else
          printf 'distillation: on\n'
        fi
        ;;
      401) printf 'server: the token was refused; run the setup command again\n' ;;
      000 | '') printf 'server: unreachable\n' ;;
      *) printf 'server: answered HTTP %s\n' "$st_code" ;;
    esac
  fi
  if [ -f "$SCRIPT" ]; then
    st_v=$(script_version "$SCRIPT")
    printf 'hook script: ~/.config/alice/alice-capture.sh (version %s; it updates itself from the server once a day' "${st_v:-unknown}"
    st_at=$(head -n 1 "$CHECKED" 2>/dev/null)
    if epoch_ok "$st_at"; then
      st_h=$((($(date +%s) - st_at) / 3600))
      if [ "$st_h" -lt 0 ]; then st_h=0; fi
      printf '; last check %s h ago)\n' "$st_h"
    else
      printf ')\n'
    fi
  else
    printf 'hook script: missing (~/.config/alice/alice-capture.sh); run the setup command\n'
  fi
  if briefing_on; then
    printf 'briefing: on (~/.config/alice/capture.conf)\n'
  else
    printf 'briefing: off (~/.config/alice/capture.conf)\n'
  fi
  st_n=0
  st_lines=
  for st_f in "$STATE"/*.offset; do
    [ -f "$st_f" ] || continue
    st_n=$((st_n + 1))
    st_key=${st_f##*/}
    st_key=${st_key%.offset}
    read_state "$st_f" || continue
    if [ -n "$s_stopped" ]; then
      st_lines="$st_lines$(printf "stopped: %s (the server's cursor is past the file's end); to start it again, delete ~/.config/alice/state/%s.offset" "$st_key" "$st_key")$NL"
    elif [ -n "$s_hold" ]; then
      st_lines="$st_lines$(printf 'waiting: %s (the server took nothing at %s bytes; sent again when it grows); to reset it, delete ~/.config/alice/state/%s.offset' "$st_key" "$s_hold" "$st_key")$NL"
    fi
  done
  printf 'transcripts followed: %s\n' "$st_n"
  printf '%s' "$st_lines"
  if paused; then
    printf 'paused until: %s (the server refused the token)\n' "$(head -n 1 "$PAUSED")"
  fi
  if [ -d "$DIR/claude-marketplace" ]; then
    printf 'plugin: alice-capture@alice-local (~/.config/alice/claude-marketplace)\n'
  else
    printf 'plugin: not set up; run the setup command\n'
  fi
  codex_status
  printf 'log: ~/.config/alice/capture.log\n'
  return 0
}

# Asks a yes/no question on the terminal ($ALICE_SETUP_ASSUME_YES=1 answers yes, for tests).
confirm() {
  if [ "${ALICE_SETUP_ASSUME_YES:-}" = 1 ]; then
    return 0
  fi
  if ! (exec </dev/tty) 2>/dev/null; then
    printf 'There is no terminal to ask on, so nothing was deleted.\n'
    return 1
  fi
  printf '%s' "$1" >/dev/tty
  IFS= read -r cf_answer </dev/tty || cf_answer=
  case $cf_answer in y | Y | yes | Yes | YES) return 0 ;; esac
  return 1
}

uninstall_main() {
  un_plugin=
  un_market=
  if command -v claude >/dev/null 2>&1; then
    if claude plugin uninstall alice-capture@alice-local --scope user </dev/null >/dev/null 2>&1 ||
      claude plugin uninstall alice-capture@alice-local </dev/null >/dev/null 2>&1; then
      un_plugin=1
    fi
    if claude plugin marketplace remove alice-local </dev/null >/dev/null 2>&1; then
      un_market=1
    fi
  fi
  if [ -n "$un_plugin" ] && [ -n "$un_market" ]; then
    printf 'Removed the alice-capture plugin and the alice-local marketplace from Claude Code.\n'
  else
    printf 'To finish removing the plugin, type these in Claude Code:\n'
    printf '  /plugin uninstall alice-capture@alice-local\n'
    printf '  /plugin marketplace remove alice-local\n'
  fi
  rm -rf "${DIR:?}/claude-marketplace"
  printf 'Now restart any open Claude Code sessions, so none still runs the removed hooks.\n'
  # Codex: remove hooks.json only when it is still exactly the file setup wrote.
  un_ch="${CODEX_HOME:-$HOME/.codex}"
  if [ -f "$DIR/codex-hooks.written" ]; then
    if [ -f "$un_ch/hooks.json" ] && cmp -s "$un_ch/hooks.json" "$DIR/codex-hooks.written"; then
      rm -f "$un_ch/hooks.json"
      printf 'Removed %s/hooks.json (restart open Codex sessions).\n' "$un_ch"
    elif [ -f "$un_ch/hooks.json" ]; then
      printf '%s/hooks.json has changed since setup wrote it, so it was kept; remove the Alice entries (hook codex) by hand.\n' "$un_ch"
    fi
    rm -f "$DIR/codex-hooks.written"
  fi
  if confirm "Delete the capture files (the hook script, state/, capture.conf, capture.log, paused-until, update-checked)? The token and url files stay. [y/N] "; then
    rm -rf "${DIR:?}/state"
    rm -f "$SCRIPT" "$CONF" "$LOG" "$PAUSED" "$CHECKED"
    printf 'Deleted the capture files. ~/.config/alice/token and url are kept: your MCP setup uses them.\n'
  else
    printf 'Kept the capture files in ~/.config/alice.\n'
  fi
  return 0
}

main() {
  case ${1:-} in
    hook)
      shift
      hook_main "$@"
      return 0
      ;;
    run)
      shift
      run_main "$@"
      return 0
      ;;
    print-hooks-json)
      shift
      print_hooks_json "$@"
      return
      ;;
    status) status_main ;;
    uninstall) uninstall_main ;;
    *)
      printf 'usage: sh ~/.config/alice/alice-capture.sh status | uninstall\n' >&2
      return 2
      ;;
  esac
}

main "$@"
