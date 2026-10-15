#!/usr/bin/env bash
# Tests for deploy/server/alice-server. Every system command (useradd, systemctl, caddy, ...) is a stub
# on PATH, and ALICE_SERVER_ROOT points every written path into a temporary directory.
# Run: bash deploy/server/tests/run.sh
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$(cd "$HERE/.." && pwd)/alice-server"

T="$(mktemp -d "${TMPDIR:-/tmp}/alice-server-test.XXXXXX")"
R="$T/root"
S="$T/stubs"
cleanup() { rm -rf -- "${T:?}"; }
trap cleanup EXIT

mkdir -p "$R/etc/alice" "$R/usr/local/bin" "$R/srv" "$S"
export ALICE_SERVER_ROOT="$R"
export ALICE_PING_TIMEOUT=1 ALICE_PING_INTERVAL=1
export PATH="$S:$PATH"
printf 'ALICE_PUBLIC_HOST=203.0.113.7\n' >"$R/etc/alice/server.env"
: >"$R/users"
: >"$R/listening"
: >"$R/calls.log"
mkdir -p "$R/proc"
printf 'processor\t: 0\nflags\t\t: fpu vme sse4_2 avx avx2 bmi2 avx512f\n' >"$R/proc/cpuinfo"

stub() { # name ; body on stdin
  { printf '#!/bin/bash\n'; cat; } >"$S/$1"
  chmod +x "$S/$1"
}

stub useradd <<'EOF'
echo "useradd $*" >>"$ALICE_SERVER_ROOT/calls.log"
for last in "$@"; do :; done
echo "$last" >>"$ALICE_SERVER_ROOT/users"
EOF
stub userdel <<'EOF'
echo "userdel $*" >>"$ALICE_SERVER_ROOT/calls.log"
grep -vx -- "$1" "$ALICE_SERVER_ROOT/users" >"$ALICE_SERVER_ROOT/users.tmp" || true
mv -- "${ALICE_SERVER_ROOT:?}/users.tmp" "${ALICE_SERVER_ROOT:?}/users"
EOF
stub id <<'EOF'
if [ "${1:-}" = "-u" ] && [ -n "${2:-}" ]; then
  grep -qx -- "$2" "$ALICE_SERVER_ROOT/users"
  exit
fi
exec /usr/bin/id "$@"
EOF
stub chown <<'EOF'
echo "chown $*" >>"$ALICE_SERVER_ROOT/calls.log"
EOF
stub systemctl <<'EOF'
echo "systemctl $*" >>"$ALICE_SERVER_ROOT/calls.log"
if [ "${1:-}" = enable ] && [ "${2:-}" = --now ]; then
  n="${3#alice@}"
  mkdir -p "$ALICE_SERVER_ROOT/srv/alice/$n"
  [ -z "${NO_TOKEN:-}" ] && printf 'tok-%s-0123456789abcdef0123456789abcdef\n' "$n" >"$ALICE_SERVER_ROOT/srv/alice/$n/token"
fi
exit 0
EOF
stub caddy <<'EOF'
echo "caddy $*" >>"$ALICE_SERVER_ROOT/calls.log"
if [ "${1:-}" = validate ]; then
  if [ -f "$ALICE_SERVER_ROOT/caddy-fail-first" ]; then rm -f -- "${ALICE_SERVER_ROOT:?}/caddy-fail-first"; exit 1; fi
  n=$(($(cat "$ALICE_SERVER_ROOT/vcount" 2>/dev/null || echo 0) + 1)); echo "$n" >"$ALICE_SERVER_ROOT/vcount"
  # CADDY_FAIL_FROM=n fails the n-th and later validations (n=2: the Caddyfile passes, the route is refused)
  if [ -n "${CADDY_FAIL_FROM:-}" ] && [ "$n" -ge "$CADDY_FAIL_FROM" ]; then exit 1; fi
  exit "${CADDY_FAIL:-0}"
fi
exit 0
EOF
stub curl <<'EOF'
echo "curl $*" >>"$ALICE_SERVER_ROOT/calls.log"
if [ "${PING_FAIL:-0}" != 0 ]; then exit "$PING_FAIL"; fi
echo '{"status":"ok","version":"0.1.0"}'
EOF
stub ss <<'EOF'
cat "$ALICE_SERVER_ROOT/listening" 2>/dev/null || true
EOF
stub sudo <<'EOF'
echo "sudo $*" >>"$ALICE_SERVER_ROOT/calls.log"
if [ "${1:-}" = -u ]; then shift 2; fi
exec "$@"
EOF
stub flock <<'EOF'
exit 0
EOF
stub df <<'EOF'
printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\n'
printf '/dev/stub 28000000 8000000 %s 30%% /\n' "${FREE_KB:-20000000}"
EOF

cat >"$R/usr/local/bin/alice" <<'EOF'
#!/bin/bash
echo "alice $* ALICE_HOME=${ALICE_HOME:-}" >>"$ALICE_SERVER_ROOT/calls.log"
case "$*" in
  "--version") echo "alice 0.1.0" ;;
  "token rotate --show")
    n=$(($(cat "$ALICE_SERVER_ROOT/rotn" 2>/dev/null || echo 0) + 1))
    echo "$n" >"$ALICE_SERVER_ROOT/rotn"
    tok="tok-rot$n-0123456789abcdef0123456789abcdef"
    printf '%s\n' "$tok" >"$ALICE_HOME/token"
    echo "token rotated"
    echo "$tok"
    ;;
  "import mempalace "*)
    pal=""
    prev=""
    for a in "$@"; do
      [ "$prev" = --palace ] && pal="$a"
      prev="$a"
    done
    if [ -d "$pal" ] && [ -f "$pal/drawer.txt" ]; then seen=yes; else seen=no; fi
    echo "palace=$pal present=$seen" >"$ALICE_SERVER_ROOT/import-seen"
    echo "import report: ok"
    exit "${IMPORT_FAIL:-0}"
    ;;
esac
exit 0
EOF
chmod +x "$R/usr/local/bin/alice"

COUNT=0
fail() { printf 'FAIL %s: %s\n' "$CURRENT" "$*" >&2; exit 1; }
pass() { COUNT=$((COUNT + 1)); printf 'ok %s\n' "$CURRENT"; }
as() { /bin/bash "$SCRIPT" "$@"; }
has() { grep -qF -- "$2" "$1" || fail "$1 lacks: $2"; }
hasnt() { if grep -qF -- "$2" "$1"; then fail "$1 contains: $2"; fi; }

TENANTS="$R/etc/alice/tenants"
SRV="$R/srv/alice"
OUT=""
RC=0
run() { RC=0; OUT=$(as "$@" 2>&1) || RC=$?; }
count_in() { printf '%s\n' "$OUT" | grep -cF -- "$1" || true; }

add_creates_everything() {
  run add ann
  [ "$RC" = 0 ] || fail "add exited $RC: $OUT"
  printf '%s\n' "$OUT" | grep -qF 'https://203.0.113.7/u/ann' || fail "no URL: $OUT"
  [ "$(count_in 'tok-ann-0123456789abcdef0123456789abcdef')" = 1 ] || fail "token not printed exactly once"
  printf '%s\n' "$OUT" | grep -qF 'alice remote set https://203.0.113.7/u/ann' || fail "no remote set line"
  FIRST_TOKEN=$(cat "$SRV/ann/token")
  local e="$TENANTS/ann.env"
  has "$e" 'ALICE_SERVER_PORT=47801'
  has "$e" 'ALICE_BACKUP_DAILY=2'
  has "$e" 'ALICE_BACKUP_WEEKLY=0'
  hasnt "$e" 'tok-'
  local u="$R/etc/systemd/system/alice@.service"
  has "$u" 'MemoryMax=3G'
  has "$u" 'IPAddressAllow=localhost'
  has "$u" 'IPAddressDeny=10.0.0.0/8 172.16.0.0/12 192.168.0.0/16 169.254.0.0/16 100.64.0.0/10 fc00::/7 fe80::/10'
  has "$u" 'ExecStart=/usr/local/bin/alice daemon run --tenant'
  local c="$R/etc/caddy/Caddyfile"
  has "$c" 'admin unix//run/caddy/admin.sock'
  has "$c" 'default_sni 203.0.113.7'
  has "$c" 'profile shortlived'
  has "$c" 'import /etc/caddy/alice.d/*.caddy'
  local s="$R/etc/caddy/alice.d/ann.caddy"
  has "$s" 'handle_path /u/ann/*'
  has "$s" '127.0.0.1:47801'
  has "$s" 'header_up Host {upstream_hostport}'
  has "$s" 'flush_interval -1'
  if grep -rqi fail2ban "$R/etc"; then fail "fail2ban under etc"; fi
  if grep -rq 'tok-' "$R/etc"; then fail "a token under etc"; fi
  pass
}

add_is_idempotent() {
  cp "$TENANTS/ann.env" "$T/ann.env.before"
  run add ann
  [ "$RC" = 0 ] || fail "second add exited $RC"
  printf '%s\n' "$OUT" | grep -qi 'exists' || fail "does not say exists: $OUT"
  printf '%s\n' "$OUT" | grep -q 'tok-' && fail "second add printed a token"
  cmp "$T/ann.env.before" "$TENANTS/ann.env" || fail "env file changed"
  pass
}

ports_skip_used() {
  echo '127.0.0.1:47802' >"$R/listening"
  run add bob
  [ "$RC" = 0 ] || fail "add bob exited $RC: $OUT"
  has "$TENANTS/bob.env" 'ALICE_SERVER_PORT=47803'
  : >"$R/listening"
  pass
}

bad_names_are_refused() {
  local n
  for n in Ann -x a_b abcdefghijklmnopq; do
    run add "$n"
    [ "$RC" != 0 ] || fail "add $n succeeded"
  done
  # exact-name listings: the Mac's filesystem is case-insensitive, so -e on Ann.env would match ann.env
  if find "$TENANTS" "$SRV" -maxdepth 1 -print | grep -E '/(Ann|a_b|-x|abcdefghijklmnopq)(\.env)?$' >/dev/null; then
    fail "something was created for a bad name"
  fi
  if grep -q 'alice-Ann\|alice-a_b\|alice-abcdefghijklmnopq' "$R/users"; then fail "a user was created"; fi
  pass
}

caddy_failure_rolls_back() {
  rm -f -- "${R:?}/vcount"
  CADDY_FAIL_FROM=2 run add carl
  [ "$RC" != 0 ] || fail "add carl succeeded with a failing caddy"
  [ ! -e "$TENANTS/carl.env" ] || fail "carl.env left behind"
  [ ! -e "$R/etc/caddy/alice.d/carl.caddy" ] || fail "carl.caddy left behind"
  has "$R/calls.log" 'userdel alice-carl'
  [ ! -e "$SRV/carl" ] || fail "carl home left behind"
  pass
}

snippet_routes_mcp() {
  local s="$R/etc/caddy/alice.d/ann.caddy"
  has "$s" '/mcp /mcp/*'
  has "$s" 'respond @admin 404'
  has "$s" 'header_up -Origin'
  pass
}

add_prints_the_mcp_url() {
  run add dave
  [ "$RC" = 0 ] || fail "add dave exited $RC: $OUT"
  printf '%s\n' "$OUT" | grep -qF 'https://203.0.113.7/u/dave/mcp' || fail "no MCP URL"
  # shellcheck disable=SC2016  # the literal text ${ALICE_TOKEN} is what must be printed
  printf '%s\n' "$OUT" | grep -qF 'Authorization: Bearer ${ALICE_TOKEN}' || fail "no literal \${ALICE_TOKEN} example"
  [ "$(count_in 'tok-dave-')" = 1 ] || fail "token not printed once"
  pass
}

rotate_prints_a_new_token() {
  run rotate ann
  [ "$RC" = 0 ] || fail "rotate exited $RC: $OUT"
  [ "$(count_in 'tok-rot1-')" = 1 ] || fail "new token not printed once: $OUT"
  printf '%s\n' "$OUT" | grep -qF "$FIRST_TOKEN" && fail "old token printed"
  has "$R/calls.log" 'sudo -u alice-ann env ALICE_HOME='
  pass
}

list_shows_and_warns() {
  run list
  [ "$RC" = 0 ] || fail "list exited $RC: $OUT"
  printf '%s\n' "$OUT" | grep -E '^ann +47801 ' | grep -qF 'https://203.0.113.7/u/ann' || fail "ann row missing: $OUT"
  printf '%s\n' "$OUT" | grep -q 'tok-' && fail "list printed a token"
  printf '%s\n' "$OUT" | grep -q WARNING && fail "warned with plenty free"
  FREE_KB=2000000 run list
  printf '%s\n' "$OUT" | grep WARNING | grep -qF '3 GB' || fail "no low-space warning: $OUT"
  pass
}

PALACE="$T/palace-src"
make_palace() { rm -rf -- "${PALACE:?}"; mkdir -p "$PALACE"; echo drawer >"$PALACE/drawer.txt"; echo db >"$PALACE/chroma.sqlite3"; }

import_moves_runs_and_cleans() {
  make_palace
  rm -f -- "${R:?}/import-seen"
  run import ann "$PALACE"
  [ "$RC" = 0 ] || fail "import exited $RC: $OUT"
  printf '%s\n' "$OUT" | grep -qF 'import report: ok' || fail "report not printed"
  has "$R/import-seen" "palace=$SRV/ann/import-inbox/"
  has "$R/import-seen" '/palace present=yes'
  has "$R/calls.log" 'import mempalace --palace '
  has "$R/calls.log" 'sudo -u alice-ann env ALICE_HOME='
  has "$R/calls.log" 'chown -R alice-ann:alice-ann'
  [ ! -e "$PALACE" ] || fail "the original palace path still exists"
  [ -z "$(ls -A "$SRV/ann/import-inbox")" ] || fail "job folder left behind"
  pass
}

import_cleans_on_failure() {
  make_palace
  IMPORT_FAIL=1 run import ann "$PALACE"
  [ "$RC" != 0 ] || fail "import succeeded though alice failed"
  [ -z "$(ls -A "$SRV/ann/import-inbox")" ] || fail "job folder left behind after failure"
  pass
}

import_refuses_a_missing_palace() {
  local before
  before=$(grep -c 'import mempalace' "$R/calls.log" || true)
  run import ann "$T/does-not-exist"
  [ "$RC" != 0 ] || fail "import of a missing palace succeeded"
  [ "$(grep -c 'import mempalace' "$R/calls.log" || true)" = "$before" ] || fail "alice import was run"
  pass
}

import_refuses_an_unknown_tenant() {
  make_palace
  run import zed "$PALACE"
  [ "$RC" != 0 ] || fail "import for an unknown tenant succeeded"
  [ -d "$PALACE" ] || fail "the palace was moved for an unknown tenant"
  pass
}

cli_runs_as_the_tenant() {
  run cli ann --version
  [ "$RC" = 0 ] || fail "cli exited $RC: $OUT"
  printf '%s\n' "$OUT" | grep -qF 'alice 0.1.0' || fail "no version output"
  has "$R/calls.log" 'sudo -u alice-ann'
  has "$R/calls.log" "ALICE_HOME=$SRV/ann"
  printf 'fake-secret-value\n' | as cli ann config set-secret typesafe >/dev/null 2>&1 || fail "set-secret <name> refused"
  has "$R/calls.log" 'config set-secret typesafe'
  pass
}

cli_refuses_a_secret_in_argv() {
  run cli ann config set-secret typesafe sk-fake-secret-value
  [ "$RC" != 0 ] || fail "a secret on the command line was accepted"
  printf '%s\n' "$OUT" | grep -qF 'a secret is never passed on the command line' || fail "no refusal text: $OUT"
  printf '%s\n' "$OUT" | grep -qF 'sk-fake-secret-value' && fail "the refusal echoed the secret"
  hasnt "$R/calls.log" 'sk-fake-secret-value'
  run cli ann config set-secret typesafe=sk-fake-secret-value
  [ "$RC" != 0 ] || fail "name=value form accepted"
  hasnt "$R/calls.log" 'sk-fake-secret-value'
  pass
}

upgrade_rolls_back_on_a_failed_ping() {
  { cat "$R/usr/local/bin/alice"; echo '# new build'; } >"$R/new-alice"
  chmod +x "$R/new-alice"
  cp -p "$R/usr/local/bin/alice" "$T/alice.saved"
  PING_FAIL=1 run upgrade "$R/new-alice"
  [ "$RC" != 0 ] || fail "upgrade succeeded with a failing ping"
  cmp "$T/alice.saved" "$R/usr/local/bin/alice" || fail "old binary not restored"
  printf '%s\n' "$OUT" | grep -qF 'ann' || fail "failed tenants not named: $OUT"
  pass
}

upgrade_installs_and_keeps_prev() {
  cp -p "$R/usr/local/bin/alice" "$T/alice.saved"
  run upgrade "$R/new-alice"
  [ "$RC" = 0 ] || fail "upgrade exited $RC: $OUT"
  cmp "$R/new-alice" "$R/usr/local/bin/alice" || fail "new binary not installed"
  cmp "$T/alice.saved" "$R/usr/local/bin/alice.prev" || fail "alice.prev is not the old binary"
  has "$R/calls.log" 'systemctl restart alice@ann'
  pass
}

refuses_to_run_with_empty_roots() {
  mkdir -p "$T/outside"
  echo keep >"$T/outside/file"
  local before
  before=$(cat "$R/calls.log" | wc -l)
  ALICE_SERVER_SRV_DIR="" run add eve
  [ "$RC" != 0 ] || fail "ran with an empty SRV"
  printf '%s\n' "$OUT" | grep -qF 'SRV' || fail "the error does not name SRV: $OUT"
  [ "$(cat "$R/calls.log" | wc -l)" = "$before" ] || fail "a command ran despite the empty root"
  [ -f "$T/outside/file" ] || fail "something outside the temp root was touched"
  [ ! -e "$TENANTS/eve.env" ] || fail "a tenant was created"
  ALICE_SERVER_SRV_DIR="" run remove ann --yes
  [ "$RC" != 0 ] || fail "remove ran with an empty SRV"
  [ -e "$TENANTS/ann.env" ] || fail "remove deleted a tenant with an empty SRV"
  pass
}

dry_run_import_keeps_the_source() {
  make_palace
  run import ann "$PALACE" --dry-run
  [ "$RC" = 0 ] || fail "dry-run exited $RC: $OUT"
  has "$R/import-seen" '/palace present=yes'
  has "$R/calls.log" '--dry-run'
  [ -f "$PALACE/drawer.txt" ] || fail "the dry run consumed the source"
  [ -z "$(ls -A "$SRV/ann/import-inbox")" ] || fail "job folder left behind"
  pass
}

import_refuses_a_tenant_home() {
  echo db >"$SRV/ann/chroma.sqlite3"
  echo x >"$SRV/ann/marker"
  run import ann "$SRV/ann"
  [ "$RC" != 0 ] || fail "imported a tenant home"
  run import ann "$SRV/ann/import-inbox/.."
  [ "$RC" != 0 ] || fail "imported a tenant home through .."
  mkdir -p "$SRV/bob"; echo db >"$SRV/bob/chroma.sqlite3"; echo x >"$SRV/bob/marker"   # bob is a real tenant: keep its home
  run import ann "$SRV/bob"
  [ "$RC" != 0 ] || fail "imported another tenant's home"
  [ -f "$SRV/ann/marker" ] && [ -f "$SRV/bob/marker" ] || fail "a home was moved or deleted"
  run import ann "$SRV"
  [ "$RC" != 0 ] || fail "imported the whole srv"
  rm -f -- "${SRV:?}/ann/chroma.sqlite3" "${SRV:?}/ann/marker"
  rm -f -- "${SRV:?}/bob/chroma.sqlite3" "${SRV:?}/bob/marker"
  pass
}

import_refuses_root_and_system_dirs() {
  local d
  for d in / // /usr /etc /var/lib /root /home "$R/etc/alice" "$R/etc/alice/"; do
    run import ann "$d"
    [ "$RC" != 0 ] || fail "import of $d succeeded"
  done
  [ -d /usr ] && [ -d /etc ] || fail "unexpected"
  pass
}

import_refuses_a_non_palace() {
  mkdir -p "$T/not-a-palace"; echo x >"$T/not-a-palace/file"
  run import ann "$T/not-a-palace"
  [ "$RC" != 0 ] || fail "imported a non-palace"
  [ -f "$T/not-a-palace/file" ] || fail "the non-palace was moved"
  pass
}

add_restores_the_caddyfile_when_validation_fails() {
  printf 'old caddyfile\n' >"$R/etc/caddy/Caddyfile"
  cp "$R/etc/caddy/Caddyfile" "$T/caddyfile.before"
  : >"$R/caddy-fail-first"
  run add frank
  [ "$RC" != 0 ] || fail "add succeeded though validation failed"
  cmp "$T/caddyfile.before" "$R/etc/caddy/Caddyfile" || fail "the Caddyfile was not restored"
  [ ! -e "$TENANTS/frank.env" ] || fail "a tenant was created"
  ls "$R/etc/caddy"/Caddyfile.prev-run.* >/dev/null 2>&1 && fail "temporary copy left behind"
  pass
}

add_reloads_after_removing_the_snippet() {
  local before after
  before=$(grep -c 'systemctl reload caddy' "$R/calls.log" || true)
  NO_TOKEN=1 run add gina
  [ "$RC" != 0 ] || fail "add succeeded without a token"
  after=$(grep -c 'systemctl reload caddy' "$R/calls.log" || true)
  [ "$((after - before))" -ge 2 ] || fail "Caddy was not reloaded after the rollback ($before -> $after)"
  [ ! -e "$R/etc/caddy/alice.d/gina.caddy" ] || fail "the snippet is still there"
  [ ! -e "$TENANTS/gina.env" ] || fail "tenant file left behind"
  pass
}

test_hooks_are_ignored_without_a_test_root() {
  # Without ALICE_SERVER_ROOT the SRV override must be ignored: the script reaches its usage error (exit 2)
  # instead of refusing an empty SRV (exit 1). "bogus" exits before any action.
  RC=0
  OUT=$(ALICE_SERVER_ROOT="" ALICE_SERVER_SRV_DIR="" ALICE_PING_TIMEOUT=0 /bin/bash "$SCRIPT" bogus 2>&1) || RC=$?
  [ "$RC" = 2 ] || fail "expected the usage exit 2, got $RC: $OUT"
  printf '%s\n' "$OUT" | grep -q 'SRV' && fail "the SRV override was honoured"
  pass
}

remove_archives_then_deletes() {
  run remove ann </dev/null
  [ "$RC" != 0 ] || fail "remove without confirmation succeeded"
  [ -e "$TENANTS/ann.env" ] || fail "declined remove deleted the tenant"
  run remove ann --yes
  [ "$RC" = 0 ] || fail "remove exited $RC: $OUT"
  ls "$SRV"/backups/ann-*.tar.gz >/dev/null 2>&1 || fail "no archive"
  [ ! -e "$SRV/ann" ] || fail "home left behind"
  [ ! -e "$TENANTS/ann.env" ] || fail "env left behind"
  [ ! -e "$R/etc/caddy/alice.d/ann.caddy" ] || fail "snippet left behind"
  has "$R/calls.log" 'userdel alice-ann'
  pass
}

is_0600() { [ -n "$(find "$1" -maxdepth 0 -perm 600)" ]; }
team_port() { sed -n 's/^ALICE_SERVER_PORT=//p' "$TENANTS/team.env"; }
PERSONAL="ann bob dave"

add_refuses_a_cpu_without_avx2() {
  local before
  cp "$R/proc/cpuinfo" "$T/cpuinfo.good"
  # avx is present, avx2 is not (avx512f must not count as either)
  printf 'processor\t: 0\nflags\t\t: fpu sse4_2 avx avx512f\n' >"$R/proc/cpuinfo"
  before=$(wc -l <"$R/calls.log")
  run add zoe
  cp "$T/cpuinfo.good" "$R/proc/cpuinfo"
  [ "$RC" != 0 ] || fail "add succeeded on a CPU without AVX2"
  printf '%s\n' "$OUT" | grep -qF "this CPU lacks AVX/AVX2, which Alice's embedding runtime needs (it would die with 'Illegal instruction'); on Proxmox set the VM's CPU type to host (or x86-64-v3), then shut the VM down and start it again (a reboot from inside the guest does not apply a CPU type change)" || fail "no CPU message: $OUT"
  [ "$(wc -l <"$R/calls.log")" = "$before" ] || fail "a command ran before the CPU refusal"
  [ ! -e "$TENANTS/zoe.env" ] || fail "a tenant was created"
  pass
}

upgrade_refuses_a_cpu_without_avx2() {
  local before
  cp "$R/proc/cpuinfo" "$T/cpuinfo.good"
  printf 'processor\t: 0\nflags\t\t: fpu sse4_2 avx2\n' >"$R/proc/cpuinfo"   # avx2 without avx still refuses
  cp -p "$R/usr/local/bin/alice" "$T/alice.saved"
  before=$(wc -l <"$R/calls.log")
  run upgrade "$R/new-alice"
  cp "$T/cpuinfo.good" "$R/proc/cpuinfo"
  [ "$RC" != 0 ] || fail "upgrade succeeded on a CPU without AVX"
  printf '%s\n' "$OUT" | grep -qF 'this CPU lacks AVX/AVX2' || fail "no CPU message: $OUT"
  [ "$(wc -l <"$R/calls.log")" = "$before" ] || fail "a command ran before the CPU refusal"
  cmp "$T/alice.saved" "$R/usr/local/bin/alice" || fail "the binary changed"
  pass
}

add_team_writes_no_snippet() {
  run add --team bob
  [ "$RC" != 0 ] || fail "--team with another name succeeded"
  run add team
  [ "$RC" != 0 ] || fail "a plain add of the name team succeeded"
  [ ! -e "$TENANTS/team.env" ] || fail "team.env created by a refused add"
  run add --team team
  [ "$RC" = 0 ] || fail "add --team exited $RC: $OUT"
  [ ! -e "$R/etc/caddy/alice.d/team.caddy" ] || fail "the team store got a Caddy snippet"
  has "$TENANTS/team.env" 'ALICE_DAEMON_ARGS=--team-store'
  hasnt "$TENANTS/team.env" 'ALICE_TENANT_NAME'
  hasnt "$TENANTS/team.env" 'ALICE_TEAM_URL'
  # shellcheck disable=SC2016  # the literal text $ALICE_DAEMON_ARGS is what the unit must hold
  has "$R/etc/systemd/system/alice@.service" 'ExecStart=/usr/local/bin/alice daemon run --tenant $ALICE_DAEMON_ARGS'
  hasnt "$TENANTS/bob.env" 'ALICE_DAEMON_ARGS'
  printf '%s\n' "$OUT" | grep -q 'https\?://' && fail "the team output prints a URL: $OUT"
  [ "$(count_in 'tok-team-0123456789abcdef0123456789abcdef')" = 1 ] || fail "team token not printed exactly once: $OUT"
  pass
}

add_team_provisions_existing_tenants() {
  local n tp
  tp=$(team_port)
  [ -n "$tp" ] || fail "no team port"
  for n in $PERSONAL; do
    has "$TENANTS/$n.env" "ALICE_TEAM_URL=http://127.0.0.1:$tp"
    has "$TENANTS/$n.env" "ALICE_TENANT_NAME=$n"
    is_0600 "$SRV/$n/team.token" || fail "$n team.token is not mode 0600"
    cmp "$SRV/team/token" "$SRV/$n/team.token" || fail "$n team.token differs from the team token"
    grep -qE "chown alice-$n:alice-$n .*$n/team.token" "$R/calls.log" || fail "no chown of $n team.token"
    has "$R/calls.log" "systemctl restart alice@$n"
  done
  # idempotent: again, with an old tenant file that lacks ALICE_TENANT_NAME
  grep -v '^ALICE_TENANT_NAME=' "$TENANTS/dave.env" >"$T/dave.env" && cp "$T/dave.env" "$TENANTS/dave.env"
  run add --team team
  [ "$RC" = 0 ] || fail "second add --team exited $RC: $OUT"
  printf '%s\n' "$OUT" | grep -q 'tok-' && fail "second add --team printed a token"
  for n in $PERSONAL; do
    [ "$(grep -c '^ALICE_TEAM_URL=' "$TENANTS/$n.env")" = 1 ] || fail "$n has not exactly one ALICE_TEAM_URL"
    [ "$(grep -c '^ALICE_TENANT_NAME=' "$TENANTS/$n.env")" = 1 ] || fail "$n has not exactly one ALICE_TENANT_NAME"
  done
  hasnt "$TENANTS/team.env" 'ALICE_TEAM_URL'
  pass
}

add_person_after_team_is_provisioned() {
  local tp l_chown l_start
  tp=$(team_port)
  run add hal
  [ "$RC" = 0 ] || fail "add hal exited $RC: $OUT"
  has "$TENANTS/hal.env" "ALICE_TEAM_URL=http://127.0.0.1:$tp"
  has "$TENANTS/hal.env" 'ALICE_TENANT_NAME=hal'
  is_0600 "$SRV/hal/team.token" || fail "hal team.token is not mode 0600"
  cmp "$SRV/team/token" "$SRV/hal/team.token" || fail "hal team.token differs"
  l_chown=$(grep -nE 'chown alice-hal:alice-hal .*hal/team.token' "$R/calls.log" | head -n 1 | cut -d: -f1)
  l_start=$(grep -n 'systemctl enable --now alice@hal' "$R/calls.log" | head -n 1 | cut -d: -f1)
  [ -n "$l_chown" ] && [ -n "$l_start" ] && [ "$l_chown" -lt "$l_start" ] || fail "team.token not in place before the first start ($l_chown, $l_start)"
  PERSONAL="ann bob dave hal"
  pass
}

rotate_team_rewrites_every_copy() {
  local n before after
  before=$(grep -c 'systemctl restart' "$R/calls.log" || true)
  run rotate team
  [ "$RC" = 0 ] || fail "rotate team exited $RC: $OUT"
  [ "$(count_in 'tok-rot2-')" = 1 ] || fail "new team token not printed once: $OUT"
  has "$SRV/team/token" 'tok-rot2-'
  for n in $PERSONAL; do
    cmp "$SRV/team/token" "$SRV/$n/team.token" || fail "$n team.token was not rewritten"
    is_0600 "$SRV/$n/team.token" || fail "$n team.token is not mode 0600"
  done
  after=$(grep -c 'systemctl restart' "$R/calls.log" || true)
  [ "$before" = "$after" ] || fail "rotate team restarted a daemon"
  pass
}

team_token_never_in_env_files() {
  local tok leaked
  tok=$(head -n 1 "$SRV/team/token")
  if grep -rqF -- "$tok" "$R/etc"; then fail "the team token is under etc"; fi
  leaked=$(grep -rlF -- "$tok" "$R" | grep -v -F -- "$SRV/" || true)
  [ -z "$leaked" ] || fail "the team token is outside the tenant homes: $leaked"
  pass
}

list_marks_the_team_store() {
  run list
  [ "$RC" = 0 ] || fail "list exited $RC: $OUT"
  printf '%s\n' "$OUT" | grep -E '^team ' | grep -qF '(team, no URL)' || fail "team row not marked: $OUT"
  printf '%s\n' "$OUT" | grep -E '^team ' | grep -qF 'https://' && fail "team row has a URL"
  printf '%s\n' "$OUT" | grep -E '^hal ' | grep -qF 'https://203.0.113.7/u/hal' || fail "hal row missing"
  pass
}

remove_person_deletes_the_team_copy() {
  local archive
  run remove hal --yes
  [ "$RC" = 0 ] || fail "remove hal exited $RC: $OUT"
  [ ! -e "$SRV/hal" ] || fail "hal home left behind"
  archive=$(find "$SRV/backups" -name 'hal-*.tar.gz' | head -n 1)
  if tar -tzf "$archive" | grep -q 'team.token'; then fail "the archive holds a team.token"; fi
  PERSONAL="ann bob dave"
  pass
}

remove_team_unprovisions() {
  local n before after
  before=$(grep -c 'systemctl restart' "$R/calls.log" || true)
  run remove team --yes
  [ "$RC" = 0 ] || fail "remove team exited $RC: $OUT"
  [ ! -e "$TENANTS/team.env" ] || fail "team.env left behind"
  [ ! -e "$SRV/team" ] || fail "team home left behind"
  for n in $PERSONAL; do
    hasnt "$TENANTS/$n.env" 'ALICE_TEAM_URL'
    has "$TENANTS/$n.env" "ALICE_TENANT_NAME=$n"
    [ ! -e "$SRV/$n/team.token" ] || fail "$n team.token left behind"
  done
  after=$(grep -c 'systemctl restart' "$R/calls.log" || true)
  [ "$((after - before))" = 3 ] || fail "expected 3 restarts, saw $((after - before))"
  pass
}

provision_refuses_a_missing_team_token() {
  run add --team team
  [ "$RC" = 0 ] || fail "add --team exited $RC: $OUT"
  : >"$SRV/team/token"
  run add ivy
  [ "$RC" != 0 ] || fail "add ivy succeeded with an empty team token"
  [ ! -e "$SRV/ivy" ] || fail "ivy was left behind"
  [ ! -e "$TENANTS/ivy.env" ] || fail "ivy.env was left behind"
  find "$SRV" -name 'team.token*' -size 0 | grep -q . && fail "an empty team.token exists"
  rm -f -- "${SRV:?}/team/token"
  run add ivy
  [ "$RC" != 0 ] || fail "add ivy succeeded with no team token"
  [ ! -e "$SRV/ivy" ] || fail "ivy was left behind (no token)"
  run remove team --yes
  [ "$RC" = 0 ] || fail "remove team exited $RC: $OUT"
  pass
}

for CURRENT in \
  add_creates_everything add_is_idempotent ports_skip_used bad_names_are_refused \
  caddy_failure_rolls_back snippet_routes_mcp add_prints_the_mcp_url rotate_prints_a_new_token \
  list_shows_and_warns import_moves_runs_and_cleans import_cleans_on_failure \
  import_refuses_a_missing_palace import_refuses_an_unknown_tenant dry_run_import_keeps_the_source \
  import_refuses_a_tenant_home import_refuses_root_and_system_dirs import_refuses_a_non_palace \
  add_restores_the_caddyfile_when_validation_fails add_reloads_after_removing_the_snippet test_hooks_are_ignored_without_a_test_root cli_runs_as_the_tenant \
  cli_refuses_a_secret_in_argv upgrade_rolls_back_on_a_failed_ping upgrade_installs_and_keeps_prev \
  add_refuses_a_cpu_without_avx2 upgrade_refuses_a_cpu_without_avx2 \
  add_team_writes_no_snippet add_team_provisions_existing_tenants add_person_after_team_is_provisioned \
  rotate_team_rewrites_every_copy team_token_never_in_env_files list_marks_the_team_store \
  remove_person_deletes_the_team_copy remove_team_unprovisions provision_refuses_a_missing_team_token \
  refuses_to_run_with_empty_roots remove_archives_then_deletes; do
  "$CURRENT"
done
echo "all $COUNT scenarios passed"
