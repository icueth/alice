#!/bin/sh
# Alice capture hook for server tenants (SP2 spec section 5.3, section 5.7).
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
#                      the background worker (no secret among its arguments)
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
#   capture.log        at most 256 KB; never the token, never transcript text
#   paused-until       when the server refused the token: no upload before this time
#
# The token leaves the shell only as a curl config line on curl's stdin (curl -q -K -), written
# by the printf builtin: never in argv, a URL, a child's environment, a log or an error.
# POSIX sh; needs curl, sed, head, tail, wc, od, ls, sort, cmp, mkdir, mv and date; git and awk are
# optional.
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

cleanup() {
  unlock
  if [ -n "$tmpd" ]; then
    rm -rf "$tmpd"
    tmpd=
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
  case $pu_until in '' | *[!0-9]*) return 1 ;; esac
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
# absent).
read_state() {
  s_off=0 s_path= s_session= s_sub= s_cwd= s_seen= s_hold= s_stopped=
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
  case $s_off in '' | *[!0-9]*) s_off=0 ;; esac
  case $s_hold in *[!0-9]*) s_hold= ;; esac
  case $s_seen in *[!0-9]*) s_seen= ;; esac
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

# Whether lock directory $1 may be broken (we tried $2 times). Its holder refreshes `at` at
# the start of every pass, and one pass (one upload) may take 5 + 300 s.
# - A live holder (kill -0 on its pid) is never broken until its `at` is 900 s old, a guard
#   against a pid reused by another process.
# - A dead or unknown holder is broken once its `at` is 360 s old.
# - A dead or unknown holder that never wrote `at` is broken after 5 tries.
lock_stale() {
  ls_at=$(cat "$1/at" 2>/dev/null)
  ls_pid=$(cat "$1/pid" 2>/dev/null)
  ls_alive=
  case $ls_pid in
    '' | *[!0-9]*) ;;
    *) if kill -0 "$ls_pid" 2>/dev/null; then ls_alive=1; fi ;;
  esac
  case $ls_at in
    '' | *[!0-9]*)
      [ -z "$ls_alive" ] && [ "$2" -gt 5 ]
      return
      ;;
  esac
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
  printf '%s\n' "$$" >"$lk_dir/pid"
  date +%s >"$lk_dir/at"
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
  date +%s >"$lockdir/at" 2>/dev/null
  return 0
}

# Removes the lock only when this process owns it.
unlock() {
  if own_lock; then
    rm -f "$lockdir/at" "$lockdir/pid"
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
  # SessionEnd closes a trailing turn the server waited on, so it may send once past a hold.
  co_bypass=
  if [ "$co_event" = session-end ]; then co_bypass=1; fi
  co_pass=0
  co_cap=$BODY_START
  co_pending=
  co_lost=
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
      # SessionEnd with nothing new (a Stop sent the rest): one empty upload at the offset, so
      # the server still ends the session and marks it for distillation.
      if [ "$co_event" = session-end ] && [ "$co_pass" -eq 1 ] && [ "$s_off" -gt 0 ]; then
        co_empty=1
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
        if [ -n "$co_empty" ]; then
          log "session $3: nothing new at the session end; the server ended the session at offset $s_off"
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
        log "session $3: sent $co_n bytes from offset $s_off ($co_event); the server's cursor is at $co_next"
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
  if confirm "Delete the capture files (the hook script, state/, capture.conf, capture.log, paused-until)? The token and url files stay. [y/N] "; then
    rm -rf "${DIR:?}/state"
    rm -f "$DIR/alice-capture.sh" "$CONF" "$LOG" "$PAUSED"
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
    status) status_main ;;
    uninstall) uninstall_main ;;
    *)
      printf 'usage: sh ~/.config/alice/alice-capture.sh status | uninstall\n' >&2
      return 2
      ;;
  esac
}

main "$@"
