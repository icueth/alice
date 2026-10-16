#!/usr/bin/env bash
# Tests for crates/alice-daemon/assets/upload.sh (the script a person runs to upload their MemPalace).
# curl and sqlite3 are fakes on a private PATH that record their arguments; HOME and TMPDIR are temporary.
# Run: bash deploy/server/tests/upload.sh
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$(cd "$HERE/../../.." && pwd)/crates/alice-daemon/assets/upload.sh"
T="$(mktemp -d "${TMPDIR:-/tmp}/alice-upload-test.XXXXXX")"
trap 'rm -rf -- "${T:?}"' EXIT

pass=0
fail=0
ok() { pass=$((pass + 1)); }
bad() { fail=$((fail + 1)); echo "FAIL: $*"; }
check() { # description ; command...
  local d=$1
  shift
  if "$@"; then ok; else bad "$d"; fi
}
contains() { grep -qF -- "$2" "$1"; }
lacks() { ! grep -qF -- "$2" "$1"; }

# A PATH with only the tools the script may use (no sqlite3, no curl), and the fakes beside it.
BIN="$T/bin"
mkdir -p "$BIN"
for t in sed grep find cp mkdir rm tar gzip mktemp df du awk stat id wc head tr chmod cat sleep ls dirname basename date mv touch sort; do
  p="$(type -P "$t")" && ln -s "$p" "$BIN/$t"
done
FAKE="$T/fake"
mkdir -p "$FAKE"
cat >"$FAKE/curl" <<'FAKE'
#!/bin/sh
# Records argv (one argument per line) and stdin; answers like the server, per FAKE_* variables.
n=$(cat "$FAKE_DIR/count" 2>/dev/null || echo 0)
n=$((n + 1))
echo $n >"$FAKE_DIR/count"
printf '%s\n' "$@" >"$FAKE_DIR/argv.$n"
cat >"$FAKE_DIR/stdin.$n"
out=
wcode=
upload=
while [ $# -gt 0 ]; do
  case $1 in
    -o) out=$2; shift ;;
    -w) wcode=$2; shift ;;
    -T) upload=$2; shift ;;
  esac
  shift
done
if [ -n "$upload" ]; then
  cp "$upload" "$FAKE_DIR/uploaded.tar.gz"
  [ -z "$out" ] || echo '{"state":"unpacking"}' >"$out"
  [ -z "$wcode" ] || printf '%s' "${FAKE_UPLOAD_CODE:-202}"
  exit "${FAKE_UPLOAD_EXIT:-0}"
fi
# status: the next line of FAKE_STATUS_FILE, the last one repeated
k=$(cat "$FAKE_DIR/statusn" 2>/dev/null || echo 0)
k=$((k + 1))
echo $k >"$FAKE_DIR/statusn"
total=$(wc -l <"$FAKE_STATUS_FILE")
[ "$k" -le "$total" ] || k=$total
sed -n "${k}p" "$FAKE_STATUS_FILE" >"$out"
printf '%s' "${FAKE_STATUS_CODE:-200}"
exit 0
FAKE
cat >"$FAKE/sqlite3" <<'FAKE'
#!/bin/sh
printf '%s\n' "$@" >>"$FAKE_DIR/sqlite3.argv"
src=$1
dst=$(printf '%s' "$2" | sed "s/^\.backup '//; s/'\$//")
cp "$src" "$dst"
FAKE
chmod +x "$FAKE/curl" "$FAKE/sqlite3"

TOKEN=tok_SECRET_0123456789abcdef
RAN=0
# newcase: a fresh HOME, TMPDIR, palace and fake dir; sets H, TMP, FAKE_DIR
newcase() {
  RAN=$((RAN + 1))
  C="$T/c$RAN"
  H="$C/home"
  TMP="$C/tmp"
  FAKE_DIR="$C/fake"
  mkdir -p "$H/.config/alice" "$H/.mempalace/palace/sub" "$TMP" "$FAKE_DIR"
  printf '%s\n' "$TOKEN" >"$H/.config/alice/token"
  chmod 600 "$H/.config/alice/token"
  printf 'chroma' >"$H/.mempalace/palace/chroma.sqlite3"
  printf 'text' >"$H/.mempalace/palace/sub/notes.txt"
  printf 'vec' >"$H/.mempalace/palace/data.bin"
  printf 'kg' >"$H/.mempalace/knowledge_graph.sqlite3"
  printf '%s\n' '{"state":"done","job_id":"j1","phase":"done","done":3,"total":3,"report":{"job_id":"j1","palace":{},"tiers":{"knowledge":5,"raw_session_chunks":7,"raw_session_transcripts":2,"raw_file_chunks":1,"checkpoint_diaries":0},"projects":{"alpha":{"memories":4,"chunks":6},"beta":{"memories":1,"chunks":1}},"wing_map":{},"duplicates":{"knowledge":0,"raw":0},"facts":{"added":9,"ended":0,"skipped":0},"drawers_seen":15,"already_imported":0,"skipped":{"empty_content":2},"blocked_by_redaction":{"github_token":1},"warnings":[],"errors":[]},"error":null,"embedding_backlog":42}' >"$C/status"
}
# run_script: args... ; with the fake PATH (WITH_SQLITE=1 adds the fake sqlite3); output in $C/out and $C/err
run_script() {
  local path
  FAKE_CURL_DIR="$T/pathdir$RAN"
  mkdir -p "$FAKE_CURL_DIR"
  [ "${WITH_CURL:-1}" = 1 ] && ln -sf "$FAKE/curl" "$FAKE_CURL_DIR/curl"
  [ "${WITH_SQLITE:-0}" = 1 ] && ln -sf "$FAKE/sqlite3" "$FAKE_CURL_DIR/sqlite3"
  path="$FAKE_CURL_DIR:$BIN"
  env -i HOME="$H" TMPDIR="$TMP" PATH="$path" FAKE_DIR="$FAKE_DIR" FAKE_STATUS_FILE="$C/status" \
    ALICE_UPLOAD_POLL_SECS=0 ${EXTRA_ENV:-} /bin/sh "$SCRIPT" "$@" >"$C/out" 2>"$C/err" </dev/null
  RC=$?; [ -n "${DEBUG:-}" ] && { cat "$C/out" "$C/err"; }
}
cat_all() { cat "$C/out" "$C/err" "$FAKE_DIR"/argv.* 2>/dev/null; }
tmp_empty() { [ -z "$(ls -A "$TMP")" ]; }
URL=https://alice.example/u/you

# --- syntax
check "sh -n" sh -n "$SCRIPT"
if command -v shellcheck >/dev/null 2>&1; then check "shellcheck" shellcheck -s sh "$SCRIPT"; fi
check "starts with #!/bin/sh" test "$(head -n 1 "$SCRIPT")" = '#!/bin/sh'

# --- happy path, no sqlite3
newcase
run_script "$URL"
check "happy: exit 0 (rc=$RC) $(cat "$C/err")" test $RC -eq 0
check "happy: token not in argv/stdout/stderr" bash -c "! cat '$C/out' '$C/err' '$FAKE_DIR'/argv.* | grep -qF '$TOKEN'"
check "happy: token on stdin of upload" contains "$FAKE_DIR/stdin.1" "Authorization: Bearer $TOKEN"
check "happy: config line shape" contains "$FAKE_DIR/stdin.1" 'header = "Authorization: Bearer '
check "happy: -K - used" contains "$FAKE_DIR/argv.1" '-K'
check "happy: --fail" contains "$FAKE_DIR/argv.1" '--fail'
check "happy: --progress-bar" contains "$FAKE_DIR/argv.1" '--progress-bar'
check "happy: content type" contains "$FAKE_DIR/argv.1" 'Content-Type: application/gzip'
check "happy: upload url" contains "$FAKE_DIR/argv.1" "$URL/api/v1/import/upload"
check "happy: status url" contains "$FAKE_DIR/argv.2" "$URL/api/v1/import/status"
check "happy: status also uses stdin config" contains "$FAKE_DIR/stdin.2" "Bearer $TOKEN"
check "happy: temp removed" tmp_empty
check "happy: warning without sqlite3" contains "$C/err" 'sqlite3 is not installed'
check "happy: report project" contains "$C/out" 'alpha: 4 memories, 6 chunks'
check "happy: report skipped why" contains "$C/out" 'empty content: 2'
check "happy: report redaction" contains "$C/out" 'github token: 1'
check "happy: report tier" contains "$C/out" 'Memories (knowledge): 5'
check "happy: backlog hint" contains "$C/out" '42 piece(s)'
check "happy: no sqlite3 call" test ! -e "$FAKE_DIR/sqlite3.argv"
mkdir -p "$C/x" && tar -xzf "$FAKE_DIR/uploaded.tar.gz" -C "$C/x"
check "happy: palace file in archive" test -f "$C/x/palace/sub/notes.txt"
check "happy: db plain-copied" test "$(cat "$C/x/palace/chroma.sqlite3")" = chroma
check "happy: kg in archive" test "$(cat "$C/x/knowledge_graph.sqlite3")" = kg

# --- sqlite3 present: .backup is used for every db, and wal files are left out
newcase
printf 'w' >"$H/.mempalace/palace/chroma.sqlite3-wal"
printf 'o' >"$H/.mempalace/palace/other.db"
WITH_SQLITE=1 run_script "$URL"
check "backup: exit 0" test $RC -eq 0
check "backup: .backup for palace db" contains "$FAKE_DIR/sqlite3.argv" ".backup '"
check "backup: three databases (2 palace + kg)" test "$(grep -c '^\.backup' "$FAKE_DIR/sqlite3.argv")" -eq 3
check "backup: no warning" lacks "$C/err" 'not installed'
mkdir -p "$C/x" && tar -xzf "$FAKE_DIR/uploaded.tar.gz" -C "$C/x"
check "backup: wal not archived" test ! -e "$C/x/palace/chroma.sqlite3-wal"
check "backup: temp removed" tmp_empty

# --- token file checks
newcase
chmod 644 "$H/.config/alice/token"
run_script "$URL"
check "mode 0644 refused" test $RC -ne 0
check "mode 0644 message" contains "$C/err" 'chmod 600'
check "mode 0644: curl never run" test ! -e "$FAKE_DIR/count"
check "mode 0644: token not printed" bash -c "! cat '$C/out' '$C/err' | grep -qF '$TOKEN'"
check "mode 0644: temp empty" tmp_empty

newcase
rm "$H/.config/alice/token"
run_script "$URL"
check "missing token refused" test $RC -ne 0
check "missing token explains" contains "$C/err" '.config/alice/token'

newcase
printf 'a b "c\n' >"$H/.config/alice/token"
run_script "$URL"
check "odd token file refused" test $RC -ne 0
check "odd token content not echoed" lacks "$C/err" 'a b'

# --- failure paths clean up
newcase
FAKE_UPLOAD_EXIT=22 FAKE_UPLOAD_CODE=409 EXTRA_ENV="FAKE_UPLOAD_EXIT=22 FAKE_UPLOAD_CODE=409" run_script "$URL"
check "curl 22: non-zero" test $RC -ne 0
check "curl 22: 409 explained" contains "$C/err" 'already importing'
check "curl 22: temp removed" tmp_empty
check "curl 22: token not printed" bash -c "! cat '$C/out' '$C/err' | grep -qF '$TOKEN'"

newcase
EXTRA_ENV="FAKE_UPLOAD_EXIT=22 FAKE_UPLOAD_CODE=401" run_script "$URL"
check "401 explained" contains "$C/err" 'did not accept your token'

newcase
printf '%s\n' '{"state":"failed","job_id":"j1","phase":"failed","done":0,"total":0,"report":null,"error":"no MemPalace found in the upload","embedding_backlog":0}' >"$C/status"
run_script "$URL"
check "failed job: non-zero" test $RC -ne 0
check "failed job: reason printed" contains "$C/err" 'no MemPalace found in the upload'
check "failed job: temp removed" tmp_empty

# --- polling: working states first, then done
newcase
{
  printf '%s\n' '{"state":"unpacking","job_id":"j1","phase":"unpacking","done":0,"total":0,"report":null,"error":null}'
  printf '%s\n' '{"state":"importing","job_id":"j1","phase":"drawers","done":1,"total":3,"report":null,"error":null}'
  cat "$C/status"
} >"$C/status2"
mv "$C/status2" "$C/status"
run_script "$URL"
check "polling: exit 0" test $RC -eq 0
check "polling: progress line" contains "$C/out" '1 of 3 drawers'
check "polling: three status calls" test "$(cat "$FAKE_DIR/statusn")" -eq 3

# --- palace auto-detect from config.json, --palace and --kg overrides
newcase
mkdir -p "$C/elsewhere/mp"
printf 'e' >"$C/elsewhere/mp/chroma.sqlite3"
printf '{ "palace_path": "%s", "x": 1 }\n' "$C/elsewhere/mp" >"$H/.mempalace/config.json"
run_script "$URL"
mkdir -p "$C/x" && tar -xzf "$FAKE_DIR/uploaded.tar.gz" -C "$C/x"
check "config.json palace used" test "$(cat "$C/x/palace/chroma.sqlite3")" = e

newcase
mkdir -p "$C/pp"
printf 'p' >"$C/pp/chroma.sqlite3"
printf 'k2' >"$C/mykg.sqlite3"
run_script "$URL" --palace "$C/pp" --kg "$C/mykg.sqlite3"
mkdir -p "$C/x" && tar -xzf "$FAKE_DIR/uploaded.tar.gz" -C "$C/x"
check "--palace used" test "$(cat "$C/x/palace/chroma.sqlite3")" = p
check "--kg used" test "$(cat "$C/x/knowledge_graph.sqlite3")" = k2

newcase
rm "$H/.mempalace/knowledge_graph.sqlite3"
run_script "$URL"
mkdir -p "$C/x" && tar -xzf "$FAKE_DIR/uploaded.tar.gz" -C "$C/x"
check "no kg: none archived" test ! -e "$C/x/knowledge_graph.sqlite3"
check "no kg: still ok" test $RC -eq 0

# --- symlinks and other oddities are skipped
newcase
ln -s /etc "$H/.mempalace/palace/sub/escape"
ln -s "$H/.mempalace/palace/data.bin" "$H/.mempalace/palace/linkfile"
ln "$H/.mempalace/palace/data.bin" "$H/.mempalace/palace/hard.bin"
run_script "$URL"
check "symlink: exit 0" test $RC -eq 0
check "symlink: warned" contains "$C/err" 'skipped 2 symbolic link'
mkdir -p "$C/x" && tar -xzf "$FAKE_DIR/uploaded.tar.gz" -C "$C/x"
check "symlink: dir link not in archive" test ! -e "$C/x/palace/sub/escape"
check "symlink: file link not in archive" test ! -e "$C/x/palace/linkfile"
check "symlink: archive holds no links at all" bash -c "! tar -tvzf '$FAKE_DIR/uploaded.tar.gz' | grep -q '^[lh]'"
check "hardlink: stored as separate regular files" bash -c "tar -tvzf '$FAKE_DIR/uploaded.tar.gz' | grep -q 'hard.bin' && ! tar -tvzf '$FAKE_DIR/uploaded.tar.gz' | grep -q 'link to'"

# --- paths it cannot pass on safely
newcase
mkdir -p "$C/it's"
printf 'x' >"$C/it's/chroma.sqlite3"
run_script "$URL" --palace "$C/it's"
check "quote in path refused" test $RC -ne 0
check "quote message" contains "$C/err" 'single quote'
check "quote: temp empty" tmp_empty

newcase
nl="$(printf 'a\nb')"
printf 'x' >"$H/.mempalace/palace/$nl"
run_script "$URL"
check "newline in name refused" test $RC -ne 0
check "newline message" contains "$C/err" 'newline'
check "newline: temp empty" tmp_empty

# --- addresses and tools
newcase
run_script "http://alice.example/u/you"
check "plain http refused" test $RC -ne 0
check "plain http: no curl" test ! -e "$FAKE_DIR/count"
newcase
run_script "http://127.0.0.1:9/u/you"
check "plain http on loopback allowed" test $RC -eq 0
newcase
run_script
check "no args: usage, exit 2" test $RC -eq 2
newcase
WITH_CURL=0 run_script "$URL"
check "no curl: refused" test $RC -ne 0
check "no curl: says so" contains "$C/err" 'curl is required'

# --- the interrupt path: a signal removes the temp folder too
newcase
cat >"$FAKE/curl-hang" <<'FAKE'
#!/bin/sh
cat >/dev/null
sleep 30
FAKE
chmod +x "$FAKE/curl-hang"
FAKE_CURL_DIR="$T/hang"
mkdir -p "$FAKE_CURL_DIR"
ln -sf "$FAKE/curl-hang" "$FAKE_CURL_DIR/curl"
env -i HOME="$H" TMPDIR="$TMP" PATH="$FAKE_CURL_DIR:$BIN" /bin/sh "$SCRIPT" "$URL" >"$C/out" 2>"$C/err" </dev/null &
pid=$!
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  [ -n "$(ls -A "$TMP" 2>/dev/null)" ] && sleep 1 && break
  sleep 0.2
done
kill -TERM "$pid" 2>/dev/null
wait "$pid" 2>/dev/null
pkill -f "$FAKE_CURL_DIR/curl" 2>/dev/null
check "SIGTERM: temp removed" tmp_empty

echo "passed $pass, failed $fail"
[ "$fail" -eq 0 ]
