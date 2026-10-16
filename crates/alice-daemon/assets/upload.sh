#!/bin/sh
# Uploads your MemPalace to your Alice tenant on a shared server.
#
#   sh upload.sh <tenant-base-url> [--palace <dir>] [--kg <file>]
#
# <tenant-base-url> is the address your admin gave you for your own tenant, for example
# https://alice.example/u/your-name (without /api or /mcp). Your token is read from
# ~/.config/alice/token (mode 0600, your user's file, the token and nothing else).
#
# What it does: copies a consistent snapshot of your MemPalace (the databases through
# sqlite3's online backup when sqlite3 is installed), packs it into one .tar.gz in a private
# temporary folder, uploads it, waits while the server imports it and prints what came of it.
# Your palace is only read, never changed. The temporary folder is removed on every exit.
#
# Needs: curl, tar, gzip. Optional: sqlite3 (without it MemPalace must be idle while this runs).
# Environment: ALICE_UPLOAD_POLL_SECS (seconds between progress checks, default 10).

set -u

PROG=alice-upload
work=
token=

say() { printf '%s\n' "$*"; }
warn() { printf '%s: warning: %s\n' "$PROG" "$*" >&2; }
die() {
  printf '%s: %s\n' "$PROG" "$*" >&2
  exit 1
}

cleanup() {
  if [ -n "$work" ] && [ -d "$work" ]; then
    rm -rf "$work"
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

usage() {
  cat >&2 <<'USAGE'
usage: sh upload.sh <tenant-base-url> [--palace <dir>] [--kg <file>]

  <tenant-base-url>  your tenant's address, for example https://alice.example/u/your-name
  --palace <dir>     your MemPalace folder (default: palace_path from ~/.mempalace/config.json,
                     else ~/.mempalace/palace)
  --kg <file>        your knowledge graph database (default: ~/.mempalace/knowledge_graph.sqlite3
                     when it exists)

The token is read from ~/.config/alice/token.
USAGE
  exit 2
}

# ---------------------------------------------------------------- arguments

base=
palace_arg=
kg_arg=
while [ $# -gt 0 ]; do
  case $1 in
    -h | --help) usage ;;
    --palace)
      [ $# -ge 2 ] || usage
      palace_arg=$2
      shift 2
      ;;
    --kg)
      [ $# -ge 2 ] || usage
      kg_arg=$2
      shift 2
      ;;
    -*) usage ;;
    *)
      [ -z "$base" ] || usage
      base=$1
      shift
      ;;
  esac
done
[ -n "$base" ] || usage

base=${base%/}
case $base in
  http:* | https:*) ;;
  *) die "the address must start with the scheme, as in https://alice.example/u/your-name (you gave: $base)" ;;
esac
case $base in
  *[!A-Za-z0-9._~:/@%+=,\[\]-]*) die "the address has characters it should not have; give it as your admin wrote it" ;;
esac
case $base in
  http:*)
    case ${base#*//} in
      127.0.0.1 | 127.0.0.1[:/]* | localhost | localhost[:/]* | \[::1\] | \[::1\][:/]*) ;;
      *) die "refusing to send your token over an unencrypted connection; use the https address" ;;
    esac
    ;;
esac

[ -n "${HOME:-}" ] || die "HOME is not set"

for tool in curl tar gzip; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool is required but was not found; install it and run this again"
done

poll=${ALICE_UPLOAD_POLL_SECS:-10}
case $poll in
  '' | *[!0-9]*) die "ALICE_UPLOAD_POLL_SECS must be a whole number of seconds" ;;
esac

# ---------------------------------------------------------------- token

token_file=$HOME/.config/alice/token
if [ ! -f "$token_file" ]; then
  cat >&2 <<EOM
$PROG: no token found at $token_file

Create that file with the token your admin gave you (the token alone, on one line), readable
only by you. For example, paste the token after running:

  mkdir -p ~/.config/alice && (umask 077; read -r T && printf '%s\n' "\$T" > ~/.config/alice/token)

See the "A person imports their own MemPalace" section of docs/server.md.
EOM
  exit 1
fi

st=$(stat -f '%Lp %u' "$token_file" 2>/dev/null)
case $st in
  [0-7][0-7][0-7]\ [0-9]* | [0-7][0-7][0-7][0-7]\ [0-9]*) ;;
  *) st=$(stat -c '%a %u' "$token_file" 2>/dev/null) ;;
esac
case $st in
  [0-7][0-7][0-7]\ [0-9]* | [0-7][0-7][0-7][0-7]\ [0-9]*) ;;
  *) die "cannot read the mode of $token_file" ;;
esac
tmode=${st% *}
towner=${st#* }
[ "$tmode" = 600 ] || die "$token_file must be readable only by you; run: chmod 600 $token_file (it is mode $tmode now)"
[ "$towner" = "$(id -u)" ] || die "$token_file must belong to the user running this"

IFS= read -r token <"$token_file" || [ -n "$token" ] || die "$token_file is empty"
token=$(printf '%s' "$token" | tr -d ' \t\r')
[ -n "$token" ] || die "$token_file is empty"
case $token in
  *[!A-Za-z0-9._~+/=-]*) die "$token_file holds more than a token (it must be the token alone, on one line)" ;;
esac

# The token reaches curl only as a config on its standard input: never in an argument, the
# environment of a child, a URL or any output.
auth_cfg() { printf 'header = "Authorization: Bearer %s"\n' "$token"; }

# ---------------------------------------------------------------- what to upload

if [ -n "$palace_arg" ]; then
  palace=$palace_arg
else
  palace=
  cfg=$HOME/.mempalace/config.json
  if [ -f "$cfg" ]; then
    palace=$(sed -n 's/.*"palace_path"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$cfg" | head -n 1 | sed 's,\\/,/,g')
    case $palace in
      \~) palace=$HOME ;;
      \~/*) palace=$HOME/${palace#\~/} ;;
    esac
  fi
  [ -n "$palace" ] || palace=$HOME/.mempalace/palace
fi
palace=${palace%/}
[ -d "$palace" ] || die "the MemPalace folder $palace does not exist; point to yours with --palace <folder>"

kg=
if [ -n "$kg_arg" ]; then
  [ -f "$kg_arg" ] || die "the knowledge graph file $kg_arg does not exist"
  kg=$kg_arg
elif [ -f "$HOME/.mempalace/knowledge_graph.sqlite3" ]; then
  kg=$HOME/.mempalace/knowledge_graph.sqlite3
fi

check_path() { # a path that goes into a sqlite3 command or a file list
  case $1 in
    *\'*) die "the path $1 has a single quote in it, which this script cannot pass on safely; rename it and run this again" ;;
  esac
}
check_path "$palace"
[ -z "$kg" ] || check_path "$kg"

if command -v sqlite3 >/dev/null 2>&1; then
  use_backup=1
else
  use_backup=0
  warn "sqlite3 is not installed, so the databases are copied as they are: make sure MemPalace (and every agent using it) is idle until this finishes"
fi

# ---------------------------------------------------------------- temporary folder

tmp_base=${TMPDIR:-/tmp}
work=$(mktemp -d "${tmp_base%/}/alice-upload.XXXXXX") || die "cannot make a temporary folder in $tmp_base"
chmod 0700 "$work" || die "cannot protect the temporary folder $work"
check_path "$work"
stage=$work/stage
mkdir "$stage" || die "cannot make $stage"

# Free space: about 1.2 times what is packed (the copy, then the archive is smaller).
size_kb=$(du -sk "$palace" 2>/dev/null | awk '{print $1}')
[ -n "$size_kb" ] || size_kb=0
if [ -n "$kg" ]; then
  kg_kb=$(du -sk "$kg" 2>/dev/null | awk '{print $1}')
  size_kb=$((size_kb + ${kg_kb:-0}))
fi
need_kb=$(((size_kb * 12 + 9) / 10))
free_kb=$(df -Pk "$work" 2>/dev/null | awk 'NR==2 {print $4}')
case $free_kb in
  '' | *[!0-9]*) warn "cannot tell how much free space $tmp_base has; continuing" ;;
  *)
    if [ "$free_kb" -lt "$need_kb" ]; then
      die "not enough free space in $tmp_base: about $((need_kb / 1024)) MB needed, $((free_kb / 1024)) MB free; set TMPDIR to a folder on a bigger disk and run this again"
    fi
    ;;
esac

# ---------------------------------------------------------------- snapshot

# A newline in a name would break the file lists below.
nl='
'
bad_name=$(cd "$palace" && find . -name "*${nl}*" | head -n 1)
[ -z "$bad_name" ] || die "a file name in $palace has a newline in it, which this script cannot pass on safely; rename it and run this again"

snapshot_db() { # source destination
  if [ "$use_backup" = 1 ]; then
    sqlite3 "$1" ".backup '$2'" || die "could not take a consistent copy of $1 (is it a database? is something holding it locked?)"
  else
    cp "$1" "$2" || die "could not copy $1"
  fi
}

say "Taking a snapshot of $palace ..."
(cd "$palace" && find . -type d) >"$work/dirs" || die "cannot read $palace"
(cd "$palace" && find . -type f) >"$work/files" || die "cannot read $palace"
(cd "$palace" && find . -type l) >"$work/links" || die "cannot read $palace"
(cd "$palace" && find . ! -type d ! -type f ! -type l) >"$work/others" || die "cannot read $palace"

mkdir "$stage/palace" || die "cannot make $stage/palace"
while IFS= read -r d; do
  mkdir -p "$stage/palace/$d" || die "cannot make a folder in the snapshot"
done <"$work/dirs"

nfiles=0
ndbs=0
while IFS= read -r f; do
  if [ "$use_backup" = 1 ]; then
    case $f in
      *.sqlite3-wal | *.sqlite3-shm | *.sqlite3-journal | *.db-wal | *.db-shm | *.db-journal) continue ;;
    esac
  fi
  case $f in
    *.sqlite3 | *.db)
      snapshot_db "$palace/$f" "$stage/palace/$f"
      ndbs=$((ndbs + 1))
      ;;
    *) cp "$palace/$f" "$stage/palace/$f" || die "could not copy $palace/$f" ;;
  esac
  nfiles=$((nfiles + 1))
done <"$work/files"

nlinks=$(wc -l <"$work/links")
nothers=$(wc -l <"$work/others")
[ $((nlinks)) -eq 0 ] || warn "skipped $((nlinks)) symbolic link(s) in the palace (links are never uploaded)"
[ $((nothers)) -eq 0 ] || warn "skipped $((nothers)) special file(s) in the palace (sockets and pipes are never uploaded)"

items=palace
if [ -n "$kg" ]; then
  case $kg in
    "$palace"/*) ;; # already in the palace folder
    *)
      snapshot_db "$kg" "$stage/knowledge_graph.sqlite3"
      ndbs=$((ndbs + 1))
      items="palace knowledge_graph.sqlite3"
      ;;
  esac
fi
say "Snapshot ready: $nfiles file(s), $ndbs of them databases."

# ---------------------------------------------------------------- pack

archive=$work/palace.tar.gz
# shellcheck disable=SC2086 # $items is two fixed words
COPYFILE_DISABLE=1 tar -czf "$archive" -C "$stage" $items || die "could not pack the snapshot"
rm -rf "$stage"
bytes=$(wc -c <"$archive")
if [ "$bytes" -ge 1048576 ]; then
  say "Packed $((bytes / 1048576)) MB. Uploading ..."
else
  say "Packed $((bytes / 1024 + 1)) KB. Uploading ..."
fi

# ---------------------------------------------------------------- upload

code=$(auth_cfg | curl --fail --progress-bar -X POST -H 'Content-Type: application/gzip' \
  -T "$archive" -o "$work/response.json" -w '%{http_code}' -K - "$base/api/v1/import/upload")
rc=$?
if [ "$rc" -ne 0 ]; then
  case $code in
    401) die "the server did not accept your token; check ~/.config/alice/token with your admin" ;;
    404) die "the server has no such address; check the tenant address you gave (it ends with your name, not /mcp)" ;;
    409) die "the server is already importing something for you (or another upload is still running); wait for it to finish, then run this again" ;;
    413) die "the archive is over the server's upload limit (4 GiB unless your admin changed it)" ;;
    400 | 507) die "the server refused the upload (most often it does not have enough free disk space for it); ask your admin" ;;
    *) die "the upload failed (curl exit code $rc, HTTP status ${code:-none}); check your connection and run this again" ;;
  esac
fi
rm -f "$archive"
say "Uploaded. The server now unpacks and imports it; this can take a while."

# ---------------------------------------------------------------- progress and report

jstr() { grep -o "\"$1\":\"[^\"]*\"" | head -n 1 | sed 's/^[^:]*:"//; s/"$//'; }
jnum() { grep -o "\"$1\":[0-9][0-9]*" | head -n 1 | sed 's/^[^:]*://'; }
jobj() { grep -o "\"$1\":{[^}]*}" | head -n 1; }
# "name":N pairs of a flat object, one per line, as "  name: N"
jpairs() { sed 's/^[^{]*{//; s/}$//' | tr ',' '\n' | sed -n 's/^"\(.*\)":\([0-9][0-9]*\)$/  \1: \2/p' | tr '_' ' '; }

print_report() { # status json file
  j=$1
  say ""
  say "Your MemPalace was imported."
  tiers=$(jobj tiers <"$j")
  say "  Drawers read: $(jnum drawers_seen <"$j")"
  say "  Memories (knowledge): $(printf '%s' "$tiers" | jnum knowledge)"
  say "  Raw session chunks: $(printf '%s' "$tiers" | jnum raw_session_chunks) (transcripts: $(printf '%s' "$tiers" | jnum raw_session_transcripts))"
  say "  Raw file chunks: $(printf '%s' "$tiers" | jnum raw_file_chunks)"
  say "  Already in Alice before (not added again): $(jnum already_imported <"$j")"
  facts=$(jobj facts <"$j")
  say "  Facts added: $(printf '%s' "$facts" | jnum added)"
  say "By project (memories and chunks):"
  sed -n 's/.*"projects":{\(.*\)},"wing_map":.*/\1/p' "$j" | head -n 1 | sed 's/},/}\
/g' | sed -n 's/^"\(.*\)":{"memories":\([0-9]*\),"chunks":\([0-9]*\)}$/  \1: \2 memories, \3 chunks/p'
  skipped=$(jobj skipped <"$j" | sed -n '/"skipped":{/p')
  if [ -n "$skipped" ] && [ "$skipped" != '"skipped":{}' ]; then
    say "Skipped, and why:"
    printf '%s\n' "$skipped" | jpairs
  fi
  held=$(jobj blocked_by_redaction <"$j")
  if [ -n "$held" ] && [ "$held" != '"blocked_by_redaction":{}' ]; then
    say "Held back because they held a secret (by kind of secret):"
    printf '%s\n' "$held" | jpairs
  fi
  backlog=$(jnum embedding_backlog <"$j")
  if [ "${backlog:-0}" -gt 0 ]; then
    say "Search by meaning is still being prepared for $backlog piece(s) of text. Searching by words works now;"
    say "meaning-based search becomes complete as the server fills them in (a few minutes to some hours)."
  fi
}

last=
fails=0
while :; do
  code=$(auth_cfg | curl -sS -o "$work/status.json" -w '%{http_code}' -K - "$base/api/v1/import/status")
  rc=$?
  if [ "$rc" -ne 0 ] || [ "$code" != 200 ]; then
    [ "$code" != 401 ] || die "the server stopped accepting your token while the import was running"
    fails=$((fails + 1))
    [ "$fails" -lt 6 ] || die "lost contact with the server (HTTP status ${code:-none}); the import may still be running there. Run this again later to see; it will report that one is running if so"
  else
    fails=0
    state=$(jstr state <"$work/status.json")
    phase=$(jstr phase <"$work/status.json")
    [ "$phase" != "$state" ] || phase=
    n=$(jnum "done" <"$work/status.json")
    t=$(jnum total <"$work/status.json")
    now="$state $phase $n $t"
    if [ "$now" != "$last" ]; then
      last=$now
      if [ "${t:-0}" -gt 0 ]; then
        say "Server: $state ${phase:+($phase) }- $n of $t drawers"
      else
        say "Server: $state${phase:+ ($phase)}"
      fi
    fi
    case $state in
      done)
        print_report "$work/status.json"
        exit 0
        ;;
      failed)
        err=$(jstr error <"$work/status.json")
        die "the server could not import it: ${err:-no reason given}. Nothing from this upload was kept. Fix what it says (or ask your admin) and run this again."
        ;;
      idle) die "the server has no import any more (it may have restarted); run this again" ;;
    esac
  fi
  sleep "$poll"
done
