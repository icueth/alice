# Alice Server Mode — Design Spec

- **Date:** 2026-10-15
- **Addresses:** the repository is public, so this spec writes `<server-ip>`, `<host>`, `<vm-lan-ip>` and `<nat-gateway>`. The real values live only in `/etc/alice/server.env` on the server and in the controller's git-ignored notes.
- **Status:** approved by the controller with binding changes (spec-review.md, 2026-10-15), applied here; base merged with
  `feat/m2-judge-settings` 9dfac19 (M2 Task 22); addendum 1 (the owner's answers: four tenants, MemPalace import through
  a client-made bundle, a fixed disk) folded in, with `feat/m3-import` 3dbfa31 merged
- **Owner:** icueth (vision); technical choices delegated to this lane
- **Lane:** server mode, worktree `project-alice-srv`, branch `feat/server-mode` (from `feat/m2-judge-settings` b28b99f)
- **Builds on:** `2026-10-13-alice-core-design.md` (the core spec), the M2 handoff `docs/superpowers/handoff/2026-10-14-m2-handoff.md`

---

## 0. Intent and scope

### 0.1 What the owner decided (vision; not re-asked)

- Alice must be usable from one server by **at most 5 people**. Each person is identified by **their own API token**, and each
  person's memory is their own. There is no shared team memory.
- The server is an Ubuntu 24.04 VM on the public IP `<server-ip>` (NAT to <vm-lan-ip>; ports 22, 80 and 443 forwarded).
  Caddy serves the bare IP; there is no domain.
- Every technical choice is delegated. This spec records each one as a ruling (§9).
- **The tenants (addendum 1):** exactly four people, `icue`, `joe-hermes`, `joe` and `petch`. Each one's memory is their own.
  They are created only in the deployment task, after its test tenants pass and are removed, and only with the controller's
  go. Token handover is the controller's call; the script prints a token once and never writes it outside the tenant's home.
- **MemPalace import is in scope (addendum 1):** each person imports their own palace into their own tenant (§3.8).
- **The disk will not be grown (addendum 1):** 28 GB root with about 19 GB free; the budget is §2.7.

### 0.2 Architecture rulings from the controller (binding; this spec fills in the details)

1. **One daemon per person.** Each daemon has its own Linux user `alice-<name>`, its own `ALICE_HOME=/srv/alice/<name>`, port and
   token. It runs under the systemd template unit `alice@<name>.service` and listens on 127.0.0.1 only.
2. **Caddy routes by path.** `https://<host>/u/<name>/…` has the prefix stripped and goes to `127.0.0.1:<port>`, with the
   upstream `Host` rewritten so the daemon's Host check (`http.rs` `Guard`) stays as it is. Streaming responses are not buffered.
   The bearer token is the only credential. (The fail2ban jail first ruled here is dropped: the server's NAT masquerades, so
   Caddy sees only the gateway's address; see §2.4.)
3. **Client remote mode.** `alice` (the CLI, `alice mcp` and the agent hooks that call the CLI) can point at a URL and a token
   instead of a local daemon. The project is detected on the client. A working tree's `.alice.toml` applies on the server exactly
   as it would locally, failing closed. Writes spool while the server is unreachable. The connection uses HTTPS, or plain HTTP to
   loopback only.
4. **The Jev key is per person and optional.** A tenant without a key runs the heuristic judge. One person's key is never shared
   with another tenant.
5. **The admin script** `deploy/server/alice-server` has the commands `add`, `remove`, `rotate`, `list` and `upgrade`. It comes with
   `docs/server.md` and a Thai section in `README.th.md`.

### 0.3 Success criteria (each one is checked in §8)

1. From a Mac, `alice remote set https://<host>/u/<name>` (token on stdin) is enough for these to work against the server:
   `alice recall`, `alice remember`, `alice context` and `alice mcp`.
2. Tenant A's token is refused on tenant B's path, and A's client never sends A's token to B's daemon.
3. A project's `.alice.toml` changes server-side behaviour exactly as it would locally: `cloud_denied` keeps its memories away
   from Jev and from remote embedders, `block_secrets` refuses writes, and custom patterns redact. This holds for later judge
   calls about that project too, and it fails closed when the rules are missing, invalid or unreadable.
4. A secret in a `remember` is redacted on the client; it never reaches the network.
5. With the server unreachable, `remember` answers "Queued" within the 3 s ping, and the queued write is stored on the server
   after a later call, once (replay is at least once, and deduplication makes the result single).
6. A tenant daemon cannot reach the LAN (the VM's private /24 and every private range), nor Caddy's admin API, whatever URL its
   settings name (R18); the public internet stays reachable for Jev and the model download.
7. `/api/v1/events/stream` delivers events through Caddy without buffering.
8. The token never appears in a log, an error message, `Debug` output, `alice remote status`, or Caddy's access log.
9. `alice import mempalace` in remote mode imports the person's own palace into their tenant:
   - the counts equal a local import of the same palace;
   - a secret-shaped drawer never appears in the uploaded bytes, and block mode holds it back on the client;
   - a rerun imports nothing new;
   - a tenant refuses an import that names a path on the server.
10. The disk stays inside its budget (§2.7):
   - uploads that would leave less than 2 GB free are refused up front;
   - each tenant keeps at most 2 scheduled backups and makes no manual ones;
   - `alice-server list` warns below 3 GB free.

### 0.4 Out of scope

Shared or team memory, accounts, a web UI, a domain, Docker, encryption at rest, and moving an existing local **Alice** home
onto a tenant. A person brings their history by importing their MemPalace palace instead (§3.8). Moving an Alice home is
possible by hand (§10, Q2) but is not built here.

---

## 1. Overview

```
 person's machine (macOS/Linux/Windows)                     server <server-ip> (Ubuntu 24.04)
 ┌───────────────────────────────────────┐                 ┌──────────────────────────────────────────────────┐
 │ agent ──stdio── alice mcp ─┐          │                 │ Caddy :443 (LE IP cert, default_sni)              │
 │ hooks ── alice context ────┤          │   HTTPS + Bearer│  /u/ann/api/v1/* ─strip, Host:127.0.0.1:47801──┐ │
 │ alice recall/remember ─────┤          │ ───────────────▶│  /u/bob/api/v1/* ─strip, Host:127.0.0.1:47802─┐│ │
 │                            ▼          │                 │  /u/*/api/v1/admin/*, /u/*/mcp, else → 404    ││ │
 │ DaemonClient (remote target)          │                 │  access log (JSON, Authorization redacted)    ││ │
 │  • detects the project locally (git)  │                 │                                               ▼▼ │
 │  • reads <root>/.alice.toml           │                 │ alice@ann.service  user alice-ann              │
 │  • redacts before sending             │                 │   alice daemon run --tenant                     │
 │  • spools when the server is away     │                 │   ALICE_HOME=/srv/alice/ann  127.0.0.1:47801   │
 │ ~/.alice/remote.json (0600)           │                 │   client-projects/<hash>/.alice.toml (mirrors) │
 │ ~/.alice/spool/                       │                 │ alice@bob.service  …                            │
 └───────────────────────────────────────┘                 └──────────────────────────────────────────────────┘
```

The server reuses the local daemon unchanged, except for **tenant mode** (§2.2): it never runs git on a path a client sent, it
uses the project identity the client sent, and it keeps a server-side copy (a *mirror*) of each project's `.alice.toml`. Every
existing privacy path reads that mirror the way it reads a working tree's file today. The client gains a **remote target** (§3)
next to the local daemon it discovers today.

---

## 2. Server side

### 2.1 One daemon per person

| Item | Value |
|---|---|
| Tenant name | `^[a-z][a-z0-9-]{0,15}$` (it goes into a user name, a URL path and a unit name) |
| Linux user | `alice-<name>`: a system user with its own group, shell `/usr/sbin/nologin`, home `/srv/alice/<name>` |
| `ALICE_HOME` | `/srv/alice/<name>`, owner `alice-<name>`, mode 0700; `/srv/alice` is root-owned, mode 0755 |
| Port | the lowest free port in 47801–47899: not in any tenant file, and not listening (`ss -ltn`) |
| Registry | `/etc/alice/tenants/<name>.env` (root, mode 0644): `ALICE_HOME`, `ALICE_SERVER_PORT`, `ALICE_LOG=info`, `ALICE_BACKUP_DAILY=2`, `ALICE_BACKUP_WEEKLY=0`. **No token, no key.** |
| Unit | `/etc/systemd/system/alice@.service` (below) |
| Binary | `/usr/local/bin/alice` (the previous one is kept as `/usr/local/bin/alice.prev`) |
| Model | each tenant downloads its own copy on first start: `multilingual-e5-small-q8`, about 118 MB. Five copies (about 0.6 GB) fit the disk, so nothing is shared |
| Logs | stderr goes to journald (`journalctl -u alice@<name>`) |

The daemon already reads `server.port` from `ALICE_SERVER_PORT` (`daemon.rs`: 0 means the setting). A fixed port is what lets
Caddy find a tenant.

```ini
# /etc/systemd/system/alice@.service (written by alice-server)
[Unit]
Description=Alice memory daemon for %i
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=alice-%i
Group=alice-%i
EnvironmentFile=/etc/alice/tenants/%i.env
ExecStart=/usr/local/bin/alice daemon run --tenant
Restart=always
RestartSec=5
TimeoutStopSec=30
UMask=0077
MemoryMax=3G
IPAddressAllow=localhost
IPAddressDeny=10.0.0.0/8 172.16.0.0/12 192.168.0.0/16 169.254.0.0/16 100.64.0.0/10 fc00::/7 fe80::/10
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/srv/alice/%i
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictSUIDSGID=yes
LockPersonality=yes

[Install]
WantedBy=multi-user.target
```

`Restart=always` is used because a clean exit, such as `POST /api/v1/admin/shutdown` from a local admin call, must not leave a
tenant down. `MemoryMax` keeps one tenant's runaway ingest from starving the others: 3 GB each, so five fit the server's
15 GB, against a measured budget of 800 MB (docs/benchmarks/m2.md). The unit passes no secret: systemd does not inherit the
admin's environment, so a `TYPESAFE_API_KEY` in an admin shell never reaches a tenant.

**No LAN access (R18).** A tenant controls its own settings through `PATCH /api/v1/settings`, including the URLs the daemon
calls. Without a limit a tenant could make its daemon reach the owner's LAN, where the Proxmox host and the gateway sit in
the VM's private /24. Three layers stop that:

1. `IPAddressAllow=localhost` with `IPAddressDeny=` every private, link-local, CGNAT and ULA range, in the unit. systemd applies
   it to every socket of the unit's processes. Localhost stays allowed: Caddy reaches the daemon there, and DNS is
   `127.0.0.53`. The public internet stays reachable for Jev and the model download.
2. Caddy's admin API moves off `localhost:2019` to a Unix socket (`admin unix//run/caddy/admin.sock`, §2.3). Localhost is
   reachable from a tenant, so a tenant URL pointed at port 2019 could otherwise stop or reload Caddy.
3. Tenant-mode validation of the URL settings (§2.2, item 6) refuses loopback, private and link-local hosts before the daemon
   ever calls them.

What stays reachable on localhost is another tenant's daemon (which refuses requests without its token), Caddy's own 80/443
(the public proxy) and the DNS stub. The deployment task lists the server's listening sockets (`ss -ltnp`) and reports
anything else.

### 2.2 Tenant mode in the daemon (`alice daemon run --tenant`)

`--tenant` sets `ServiceOptions::tenant = true` (and `DaemonOptions::tenant`). It is a process flag, not a setting, so a client
cannot switch it off through `PATCH /api/v1/settings`. Its effects:

1. **No git on client paths.** A request's `cwd` is ignored. The project comes from the explicit `project` slug, else from
   `client_project` (§4.1), else `global`. Without this, a request carrying a Mac path would make the daemon run git on a
   directory that does not exist on the server, and the memory would land silently in `global`.
2. **The project identity comes from the client.** For a request with `client_project`, the daemon validates it (§4.1),
   updates the project's mirror (item 3), and continues with
   `ProjectChoice::Detected(ProjectRef { key, slug, name, remote, root: Some(<mirror dir>) })`. From there the existing code
   runs unchanged: `upsert_project` stores the mirror directory as `root_path` (it exists, so it replaces any older root), and
   `project_root_in`, `Redactors::for_root`, `SettingsHandle::cloud_allowed`, `DaemonJevPolicy::lookup` and `query_cloud` (the
   rule shared by `query_for_remote` and, since M2 Task 22, by a precise recall from a never-stored tree) all read
   `<mirror dir>/.alice.toml` the way they read a working tree's file today. A project the store never saw is "never stored"
   only on its first request: `query_cloud` then judges it by the mirror (its `root`), exactly as Task 22 judges a local tree.
3. **The mirror.** Each project has `<ALICE_HOME>/client-projects/<h>/.alice.toml`, where `<h>` is the first 32 hex characters
   of `SHA-256(key)`. The directory name never comes from a client string. Directories are mode 0700 and files 0600. On every
   request that carries `client_project`:
   - `rules.state = "file"`: if the text differs from the mirror's, it is written to a temporary file, synced and renamed over
     the mirror.
   - `"none"`: the mirror file is removed (the directory stays).
   - `"unreadable"`: the mirror gets a marker file: a first line `# alice: the client could not read .alice.toml (<reason>)`,
     and nothing else that parses. `parse_project` recognises the marker and reports
     "the client could not read this project's .alice.toml: <reason>". `ProjectSettingsCache` then fails closed on the parse
     error, as it would for a broken local file.

   The loosening gate (`LooseningGate`, three looks at least `SETTINGS_POLL` apart) applies to the mirror as it does to a local
   file. A weaker or deleted file on the client takes effect on the server only after three such looks. Until then the stronger
   rules stay in force.
4. **A stored root that does not exist fails closed.** In tenant mode, `SettingsHandle::{cloud_allowed, project_privacy,
   project_fail_closed}` treat a `root` whose directory is missing as an unreadable project file (`block_secrets` and
   `cloud_denied` on). A missing directory means rules this machine cannot know: a mirror an admin deleted, or a project in a
   database restored from a laptop. Locally, a missing root still means "no project file", as today.
5. **Ping capabilities.** `/api/v1/ping` answers `capabilities: ["client-project", "tenant"]` (§4.2). Every daemon built from
   this branch, tenant or not, lists `"client-project"`.
6. **Settings that name a URL or a path.** A tenant changes its own settings through `PATCH /api/v1/settings`. Tenant mode
   adds a validation layer (`Settings::validate_tenant(home)`), run on every patch and on every load of `config.toml`. A
   violation is refused like any other invalid value: the patch is rejected, or the file is not applied and the last valid
   settings stay. These are all the settings that name a URL or a path, and what tenant mode does with each:

   | Setting | What it names | Tenant mode |
   |---|---|---|
   | `storage.data_dir` | the data directory | already read-only (`READ_ONLY_KEYS`); `ALICE_HOME` from the unit decides |
   | `backup.dir` | where backups are written and listed | must resolve inside `ALICE_HOME` (after `..` and symlinks); refused otherwise |
   | `embedding.endpoint` | an HTTP provider's base URL | `https://` only; a host that is `localhost` or an IP literal in a loopback, private, link-local, CGNAT or ULA range is refused; R18 is the second layer |
   | `embedding.provider = "ollama"` | a local or LAN Ollama server | refused: the server runs none, and a LAN one is what R18 forbids |
   | `judge.jev.endpoint` | the Jev API | same rule as `embedding.endpoint` |
   | `sync.remote` | the git remote of M4 sync | refused while set: sync is not built yet, and a git remote can name a LAN host or a local path; M4 decides its tenant behaviour |
   | `server.port` | the daemon's port | refused: `ALICE_SERVER_PORT` from the unit wins over the file anyway, and the admin owns it |
   | `secrets.backend` | where keys are kept | must stay `file`: there is no keychain on the server |

   No other setting names a URL or a path. The `distill.*` settings name a provider and a model, both unused until SP2, and
   SP2 must add its endpoint to this table. The rest are numbers, switches, durations and patterns. systemd's
   `ProtectSystem=strict` with `ReadWritePaths=/srv/alice/%i` stays as the second layer for paths, and R18 for URLs.
   `backup.daily` and `backup.weekly` are pinned by the unit's environment (`ALICE_BACKUP_DAILY=2`,
   `ALICE_BACKUP_WEEKLY=0`; the environment wins over the file), so a tenant's patch of them has no effect, and the patch
   answer says the variable overrides it (§2.7).
7. **What only the admin may do on the server's disk:**
   - `POST /api/v1/backups` (a manual backup) is refused: "on a server tenant backups are scheduled (the last 2 are kept);
     ask the server's admin for an extra copy". Manual backups are kept apart from the rotation (up to 5), which the disk
     budget cannot afford.
   - An import that names a palace path, or none, is refused. A tenant imports only a bundle it uploaded (§3.8).

Nothing else changes. The token, the Host and Origin guard, the ping proof, the rotation, the spool, the writer and the judge
worker stay as they are.

### 2.3 Caddy

`alice-server` manages `/etc/caddy/Caddyfile` from one template. The template reproduces the controller's settings: the global
`default_sni <server-ip>`, and the site's `tls { issuer acme { profile shortlived } }` with no contact email. On first use the
script copies the existing file to `/etc/caddy/Caddyfile.pre-alice` (once). The deployment task diffs the two before the switch.

```caddyfile
{
	default_sni <server-ip>
	admin unix//run/caddy/admin.sock
}

https://<server-ip> {
	tls {
		issuer acme {
			profile shortlived
		}
	}
	log {
		output file /var/log/caddy/alice-access.log {
			roll_size 10MiB
			roll_keep 5
		}
		format json
	}
	import /etc/caddy/alice.d/*.caddy
	handle {
		respond "Alice server" 404
	}
}
```

Each tenant gets `/etc/caddy/alice.d/<name>.caddy`:

```caddyfile
handle_path /u/<name>/* {
	route {
		@admin path /api/v1/admin/*
		respond @admin 404
		@api path /api/v1/*
		reverse_proxy @api 127.0.0.1:<port> {
			header_up Host {upstream_hostport}
			header_up -Origin
			header_up -X-Forwarded-For
			flush_interval -1
		}
		respond 404
	}
}
```

- `handle_path` strips `/u/<name>`. `route` keeps the order as written.
- Only `/api/v1/*` is exposed. `/mcp` is not routed. A remote agent goes through `alice mcp` on its own machine: that is where
  the project is detected and text is redacted (Ruling R4). `/api/v1/admin/*` (shutdown, token rotation) is not routed either:
  a rotation over the WAN would lock the person out, because the new token is never returned (Ruling R5).
- `header_up Host {upstream_hostport}` sends `127.0.0.1:<port>`, which the daemon's `Guard` accepts. `-Origin` drops any
  Origin header a browser might add.
- `flush_interval -1` flushes every write at once, so `/api/v1/events/stream` (SSE) is not buffered. The daemon already sends a
  keep-alive comment every 15 s.
- Caddy redacts `Authorization` in access logs unless `log_credentials` is set. The template never sets it, and the deployment
  task checks the log for the token (§8.4).
- Every change runs `caddy validate --config /etc/caddy/Caddyfile`, then `systemctl reload caddy`. If validation fails, the
  change is undone and the script exits non-zero.
- `admin unix//run/caddy/admin.sock` takes Caddy's admin API off `localhost:2019` (R18). The packaged unit's `caddy reload`
  reads the admin address from the Caddyfile, so reloads keep working; the deployment task checks one reload and that
  nothing listens on 2019 afterwards.

### 2.4 No per-IP banning (the NAT masquerades)

The controller measured that every connection reaches the VM from the NAT gateway `<nat-gateway>`: SSH, HTTPS and Let's
Encrypt's tls-alpn-01 checks alike. A ban would ban the gateway, which would lock everyone out and stop the IP certificate's
renewal. So `alice-server` installs **no** fail2ban jail, and the controller has disabled fail2ban server-wide.

The protection is the 256-bit token. A well-behaved client also never produces a 401: it sends its token only after the ping
proof verified (§3.2), and the public ping never answers 401.

An admin whose NAT preserves client addresses may add a jail by hand. `docs/server.md` gives this filter as a **disabled**
example (it is not installed):

```ini
# /etc/fail2ban/filter.d/alice-caddy.conf (example; needs real client addresses in the log)
[Definition]
failregex = ^.*"client_ip":"<ADDR>".*"status":401\b
datepattern = "ts":{EPOCH}
```

The deployment task still records one access-log line's `client_ip`, for the record.

### 2.5 The admin script `deploy/server/alice-server`

The script is bash, `set -euo pipefail`, and must pass `shellcheck`. It runs as root (via sudo) and takes a lock
(`flock /run/alice-server.lock`). It is idempotent: running a command twice leaves the same state and says so. It is installed
as `/usr/local/sbin/alice-server`. The unit and the Caddy template live inside it as heredocs, so the script
is the single source. The public host comes from `/etc/alice/server.env` (`ALICE_PUBLIC_HOST=<server-ip>`), so the IP is
not hard-coded. Each command below is followed by what it does.

**`add <name>`**
1. Makes sure the base exists: directories, the unit, and the Caddyfile with its import and admin socket.
2. If the tenant exists, prints its URL (not the token) and stops: "exists; `alice-server rotate <name>` gives a new token".
3. Picks a port. Creates the user and the home (0700). Writes the tenant file.
4. Runs `systemctl enable --now alice@<name>` and waits up to 180 s for `GET http://127.0.0.1:<port>/api/v1/ping` (with
   `Host: 127.0.0.1:<port>`). The first start downloads the model.
5. Writes the Caddy snippet, then validates and reloads Caddy.
6. Prints, once: the URL `https://<host>/u/<name>`, the token (read from `<home>/token`), and the client command
   `alice remote set https://<host>/u/<name>` with a note to paste the token when asked.

On failure it undoes what it created, in reverse order.

**`remove <name> [--yes]`**
Asks for confirmation unless `--yes`. Stops and disables the unit, removes the snippet and reloads Caddy. Archives the home to
`/srv/alice/backups/<name>-<UTC timestamp>.tar.gz` (root, 0600). Then removes the user, the home (`rm -rf "${home:?}"`) and the
tenant file.

**`rotate <name>`**
Runs `sudo -u alice-<name> env ALICE_HOME=<home> ALICE_NO_AUTOSTART=1 alice token rotate --show`. With the daemon up this goes
through the local API (the old token is refused at once); otherwise it rewrites the file under the home lock. Prints the new
token once, with the `alice remote set` line.

**`list`**
One row per tenant: name, port, unit state, the version the ping reports, URL and home size (its backups included). Then a
last line with the free space on the disk that holds `/srv/alice`, and the size of the `remove` archives in
`/srv/alice/backups`. It prints a warning line when less than 3 GB is free (§2.7). No token.

**`upgrade <binary>`**
1. Checks that `<binary> --version` runs.
2. Copies the current binary to `alice.prev`. Installs the new one through a temporary file and `mv`.
3. Restarts every `alice@*` unit and waits up to 120 s for each ping.
4. If any tenant fails to come back: restores `alice.prev`, restarts everything again, and exits non-zero with the names that
   failed.

### 2.6 Jev per tenant

Nothing is configured centrally. A person turns Jev on for their own tenant from their own client:
`alice config set-secret typesafe`, then `alice config set judge.provider jev`. Both go through `PUT /api/v1/secrets/typesafe`
and `PATCH /api/v1/settings` to that tenant's `secrets.toml` (the `file` backend, 0600, owned by `alice-<name>`). The tenant's
`judge.daily_budget_usd` caps the spend. Other tenants cannot read the file: separate Unix users, 0700 homes. The default
judge stays `heuristic` (`JudgeSettings::default`), so a tenant without a key gets the fast recall.

### 2.7 Disk budget (the disk is not grown)

The root filesystem is 28 GB with about 19 GB free (measured by the controller). The rules below keep five sources of disk
use inside it.

| Use | Size | Rule |
|---|---|---|
| The build (R16) | about 6 GB while it runs | Built **before** any tenant or import exists. Right after the build, `target/`, the cargo registry and git caches, the checkout and the rustup toolchain are removed; only `/usr/local/bin/alice` stays. `df -h` is recorded before and after |
| A tenant's home | up to about 2 GB (the owner's palace of 1.6 GB gave a 1.1 GB home with partial embedding; about 2 GB fully embedded), plus the 118 MB model | Shown per tenant by `alice-server list` |
| Scheduled backups | one copy of `alice.db` each (vectors and model are not backed up); about 1–1.5 GB for an owner-sized tenant | `ALICE_BACKUP_DAILY=2`, `ALICE_BACKUP_WEEKLY=0` in the unit's environment: at most 2 per tenant. The backup job's own free-space check (`space_needed`) skips a backup that has no room |
| Manual backups | — | Refused on a tenant (§2.2 item 7) |
| Pre-migration copies | one copy of `alice.db` per tenant, kept 30 days by the existing rotation | Only when an upgrade migrates the schema; `alice-server upgrade` prints the free space before it restarts the tenants |
| An import | the uploaded bundle (text and graph only, no vectors; estimated 0.3–1 GB for an owner-sized palace) while the job runs; the database grows by what is imported | One import per tenant at a time. An upload is refused up front when its declared size would leave less than **2 GB** free, or when it exceeds the 4 GiB cap. The bundle is deleted when the job ends, however it ends. Uploads never started are swept after 1 hour |
| `remove` archives | a gzip of the home | Counted by `alice-server list`; deleting old archives is the admin's job (`docs/server.md`) |

**Expected total:**
- the owner's tenant: about 2 GB home plus 2 backups of about 1.2 GB = about 4.4 GB;
- three tenants a quarter of that size: about 1.1 GB each;
- together about 8 GB of the 19 GB.

**Worst case:** four owner-sized tenants come to about 17.6 GB, which does not fit with a 3 GB reserve. The guards then act in
this order:
1. `alice-server list` warns below 3 GB free;
2. uploads are refused below the 2 GB line;
3. the backup job skips copies it has no room for;
4. the daemon's existing disk-full behaviour (read-only, writes spooled) is the last line.

The admin's lever is one line per tenant: `ALICE_BACKUP_DAILY=1` in that tenant's file, then a restart of that tenant.

---

## 3. Client remote mode

### 3.1 Configuration

| Source | Wins when | Content |
|---|---|---|
| `ALICE_REMOTE_URL` + `ALICE_REMOTE_TOKEN` | both are set and non-empty (only one set is an error naming the other) | headless use (CI, containers) |
| `<ALICE_HOME>/remote.json` | no variables | `{"url": "...", "token": "..."}`, mode 0600 |
| neither | — | local mode, as today |

- `remote.json` is written like the token file: a temporary file created with `create_new` and mode 0600, synced, renamed. On
  read it is made 0600 again; if that fails, the command stops with a message naming the file. On Windows the home directory's
  ACL protects it, as it protects the token file today.
- The two variables join `NON_SETTING_ENV`, so they never show up as unknown `ALICE_*` warnings.
- **URL rules:** `https://host[:port][/path]`, or `http://` only when the host is `127.0.0.1`, `::1` or `localhost` (tests). No
  user info, query or fragment. The trailing `/` is dropped. Anything else is refused, and the URL is quoted in the message
  (the token never is).
- **`alice remote set <url>`** reads the token from stdin when stdin is not a terminal, or else prompts with echo off (the
  existing `read_secret`). It verifies before saving: the ping proof (§3.2), the `client-project` capability, and
  `GET /api/v1/health` with the token. Only then does it write `remote.json`. It reports:
  - the server's version;
  - the writes queued in `spool/` that will now go to the server;
  - a warning if a local daemon is running or an `alice service` is installed (their memory stays on this machine).
  
  If the environment variables are set, it warns that they override the file.
- **`alice remote status`** shows the URL, where the setting comes from (file or environment), whether the server answered and
  proved the token, the server's version and capabilities, the round-trip time, and the queued writes. It never shows the token.
  Exit code 0 means connected and 1 means not, so a hook can test it.
- **`alice remote unset`** removes `remote.json`. Queued writes stay and go to whichever target is in force next.

### 3.2 Connecting

`DaemonClient` gains a target: `Local` (today's code path, untouched) or `Remote { base }`. `DaemonClient::try_connect`,
`try_existing` and `connect` choose it from §3.1. In remote mode:

- **No discovery and no spawn.** `daemon.json` is never read, `Launcher` is ignored, and no daemon is started.
- **The proof comes before the token.** The client sends `GET <base>/api/v1/ping?nonce=<fresh>`, without the token. It requires
  `service == "alice"`, the `client-project` capability, and
  `proof == ping_proof(token, nonce, answer.instance)`. The instance is the one the answer gives; with no `daemon.json` there is
  nothing else to compare. TLS authenticates the host, and the proof authenticates the tenant: tenant B's daemon cannot make A's
  proof, so a wrong `/u/<name>` never receives A's token. A server without the capability is refused with "the server runs an
  older Alice; ask its admin to upgrade". Without that check, an older server would silently ignore `client_project` and file
  everything under `global` without the project's rules.
- **One check per process.** A verified remote client is reused without a ping before every call. A WAN round trip per tool call
  would double latency. A call that fails with a transport error or 401 drops the cached client, and the next call verifies
  again. `is_live` answers from that state without a ping; `alice remote status` pings afresh (`server_ping`).
- **HTTP client.** The same reqwest/rustls stack the Jev and embedding clients use, with the system trust store through
  reqwest's platform verifier. The server's Let's Encrypt certificate is an IP certificate and chains to ISRG Root X1.
  - No redirects (`Policy::none()`): a redirect could carry the token elsewhere.
  - System proxy settings are honoured for `https://`. TLS hides the token from a CONNECT proxy, and some networks require one.
  - `http://` loopback uses no proxy, as today.
  - User agent `alice/<version>`.
- **Timeouts suit a WAN:**

  | Call | Local (today) | Remote |
  |---|---|---|
  | ping and proof, connecting included (first call of a process, re-verification) | 300 ms | 3 s |
  | TCP + TLS connect on a later call | — | 5 s |
  | wait before a write is spooled | 1.5 s (daemon start) | the ping's 3 s |
  | recall, remember, get, update, facts, undo, reviews | 5–10 s | 15 s |
  | context | 3 s | 8 s |
  | ingest | 30 s | 60 s |
  | `mcp.admin_tools` flag | 800 ms | 3 s |
  | backup now, maintenance run | 3 h | 3 h |

- **Errors** keep the `"memory daemon "` prefix contract (`UNAVAILABLE`), so writes still spool on them. A new
  `Unavailable::Remote(String)` adds the URL and the reason:
  - "the memory server https://… is unreachable: …";
  - "… did not prove it holds this token; check the URL (…/u/<name>) and the token (`alice remote set`)";
  - "… refused the token; ask the server's admin for a new one".
  
  Its `queued_note` says the write is sent when the server is back. 502, 503 and 504 from Caddy (tenant down) count as
  unreachable.

### 3.3 Project identity on the client

Wherever a request carries `cwd` (`RecallRequest`, `ContextRequest`, `RememberRequest`, `IngestRequest`), the remote client
prepares it before sending:

1. If the request names no `project`, it runs `alice_core::project::detect_project(cwd)` on the client, on the blocking pool,
   with a 60 s per-cwd cache in long-lived processes such as `alice mcp`, as the daemon's resolver does.
2. A non-global result becomes `client_project`: `key`, `slug`, `name` and `remote` from the `ProjectRef`, and `rules` from
   reading `<root>/.alice.toml`. A missing file is `none`, a read error is `unreadable` with its reason, and otherwise `file`
   with the text, up to 64 KiB (a larger file counts as `unreadable`). A `global` result sends no `client_project`.
3. `cwd` is removed. No local path is sent except inside the project key of a repository without a remote, which is
   `path:<dir>` by design.

### 3.4 Privacy: redact on the client, then the server's rules on top

The remote client redacts every text field before the request leaves the machine. It uses `spool_redactor` (`client.rs`), which
is what spooled writes already get:

- the client's `config.toml` and `ALICE_*` privacy settings, failing closed when degraded;
- the project's `.alice.toml` when the project was detected (§3.3); a named project gets the global rules only, as for a spooled
  write today;
- `privacy.redact` forced on. Text that leaves the machine is redacted even when a person stores raw text locally, the same
  principle as `Redactors::for_root_always_on` for the Jev judge.

The fields, by request:

| Request | Fields | How |
|---|---|---|
| `remember` | content, title, topics | `check`: block mode refuses here, naming the detector |
| `ingest` | source uri, chunk texts | `check` |
| `update` | title, body, topics | `check`, with the rules of the project of the client's current directory |
| `facts/assert` | subject, predicate, object | `check`, with the rules of the project of the client's current directory |
| `recall` | query | `apply` (redacts, never refuses) |

The server then applies its own rules again, as it does for every request: the tenant's `config.toml` plus the project's
mirror. Redaction is idempotent. The two layers cover each other:

- The client layer covers the network and the server's disk.
- The server layer covers a tenant's own custom patterns, which the client does not know, and the loosening gate. A half-saved
  `.alice.toml` read by the client once is a single weak read; the server still holds the last good rules.

Later judge calls about a project (the judge worker, the precise recall's Jev call, a remote embedder's query) use
`DaemonJevPolicy` and `query_cloud`. Both resolve the stored root, which is the mirror, so they follow the rules the client
last sent. **Known window:** when a person tightens `.alice.toml`, the server learns of it on that project's next request.
Memories already queued for judging can still be judged under the older rules until then. New writes cannot: their request
updates the mirror before the write is applied.

### 3.5 Spool and replay

- A remote write that cannot be sent (§3.2 errors, or a 503 `read_only`) is prepared first: identity resolved, text redacted,
  and an `idempotency_key` set if absent. It is then appended to the client's `spool/`, the existing format and file modes,
  with `client_project` and no `cwd`. A replay therefore files it under the project it was written in, even if the working tree
  has moved since.
- **Replay** is new on the client, because no local daemon will drain the spool. `spool::drain` becomes generic over a sink
  (`trait SpoolSink { remember, ingest, rules_fail_closed }`). The daemon implements the sink with `Service` and the remote
  client with HTTP. The ordering and rejection rules stay as they are:
  - entries are replayed oldest first;
  - `invalid` and `not_found` move to `rejected/`;
  - a block refusal is withheld;
  - a transport error stops the replay and keeps the order.
- **Who replays.** `alice mcp` runs a background replay every 30 s while entries are pending and the server answers. A one-shot
  command replays after its own call, within a 2 s budget; the rest waits for the next one. `alice remote status` reports what
  is pending.
- **One replayer at a time.** An advisory lock `spool/.replay.lock`, taken without waiting; a process that cannot take it skips.
  Delivery is at least once. A replay after a lost response is harmless: `remember` deduplicates by `idempotency_key` and by
  content, and `ingest` skips chunks it already holds.
- **Refusals under fail-closed rules.** The error body gains `"retry_later": true` when the daemon refused a write in block
  mode while that project's rules were fail-closed (`Service::rules_fail_closed` with the request's scope). The client keeps
  such an entry instead of withholding it, exactly as the daemon's own drain does today.
- Entries spooled in local mode (with `cwd`) are prepared at replay time, so switching a machine to remote mode carries its
  queued writes to the server under the right project.

### 3.6 Commands in remote mode

| Command | Remote behaviour |
|---|---|
| recall, remember, context, get, update, facts, fact, history, undo, review, stats, projects, status, mcp | through the server (§3.2–3.5) |
| config list/get/set/unset/set-secret/delete-secret/test | the tenant's settings and secrets on the server |
| backup list, maintenance status/run | on the server (the tenant's own backups) |
| backup now | refused by the server on a tenant (§2.2 item 7) |
| import mempalace | builds, uploads and imports a redacted bundle (§3.8) |
| import status | the server's import status |
| doctor | remote checks: `remote.json` mode, URL scheme, ping proof, capabilities, version skew, queued writes; then the server's `/api/v1/doctor`. Local daemon checks are skipped |
| config edit | refused: "this client uses the memory server <url>; change settings with `alice config set`" |
| daemon start/stop/status/logs | refused: "… uses the memory server <url>; `alice remote unset` to use a local daemon" |
| token rotate | refused: "the token of a server tenant is rotated by its admin (`alice-server rotate <name>`); then run `alice remote set`" |
| restore | refused: it replaces local files. A tenant's restore is an admin task on the server |
| service install/uninstall | refused (a login service for a local daemon); `service status` reports remote mode |
| daemon run, model fetch, bench | unchanged (explicit local operations; `daemon run` is what the server units run) |

### 3.7 `alice mcp`

`RemoteBackend` already goes through `DaemonClient`, so it gets the remote target. Project detection uses the client's first
root or the shim's cwd, as today, followed by §3.3. The admin-tools flag is read from the server with the remote timeout. The
background replay of §3.5 runs inside it.

### 3.8 Import from MemPalace (addendum 1)

The importer of `feat/m3-import` is a daemon job. It snapshots a palace **on the daemon's disk** (`importer::run`: snapshot,
survey, then the knowledge, raw, graph, collapsed, mentions and done passes), redacts everything it plans with `RedactFilter`,
and hands batches to the writer. A remote client's palace is not on the server, so (ruling R20):

**On the client: a redacted bundle.** `alice import mempalace [--palace …]` in remote mode:
1. Resolves the palace as today (`resolve_palace`) and snapshots it into the client's own `import-tmp/<id>/`, with the audited
   `snapshot()` (read-only, never modifies the palace).
2. Writes one SQLite file, `bundle.sqlite3`, from the snapshot:
   - **Drawer text:** goes through `RedactFilter::filter("drawer", text)`. The redactor is the client's global privacy
     rules (`config.toml` and `ALICE_*`) with redaction forced on, the same rule M3 uses locally
     (`for_root_always_on(None)`). In block mode a drawer whose text holds a secret is **held back**: it is not written, and
     it is counted and reported on the client.
   - **Drawer ids** become `RedactFilter::legacy_key(id)`, everywhere they appear (drawers, graph sources, tunnel
     endpoints), so references stay consistent and two different secrets stay two ids.
   - **Every other string goes through `RedactFilter::quiet`:** wing, room, hall, source file, agent, topic and the other
     metadata strings; entity names and ids; triple ids, predicates and objects; every string of the tunnels file.
   - **Graph facts:** a triple whose subject, predicate or object block mode would refuse is held back too.
   - **The rest:** the closets count and the snapshot's notes go into the bundle's metadata.
3. Deletes the snapshot copy, then uploads the bundle (§4.5). It prints the size first, then progress.
4. Starts the server's import with `ImportRequest { bundle: Some(<id>), palace: None, … }` (dry run, `--since`,
   `--map-wing`, `--collection` and `--allow-remote-embedding` pass through unchanged). Then it polls `/api/v1/import/status`
   and prints the existing report. A last line gives the drawers and facts held back on this machine.
5. Deletes its local bundle. A dry run uploads too, because the server is what plans.

**Project identity.** M3's wing map is name-based: it uses the palace's own wing list, not git and not `.alice.toml`. An
imported project is a placeholder (`key = import:<slug>`, no root) under the global rules, exactly as in a local import.
The first `client_project` with the same slug **adopts** the placeholder (`upsert_project`): it takes the client's key and
the mirror as its root, so from then on the working tree's `.alice.toml` applies, again exactly as locally. The bundle
therefore carries wing names (redacted), not one `ClientProject` per wing, and no new privacy rule is needed.

**On the server: a third backend.** Changes to M3's code are additive:
- `Backend::Bundle` in `alice_core::import::palace`, with a `BundleReader` that implements `PalaceReader` (page by id, `get`,
  `wing_counts`, `since` on `filed_at`, the same semantics as the sqlite_exact reader);
- an arm for it in `open_snapshot` and `count_closets` (the closets count comes from the bundle's metadata);
- `ImportRequest::bundle: Option<String>`;
- in `importer::run`, the snapshot step becomes "use the uploaded bundle as the snapshot" when `bundle` is set: the job id
  is the bundle id, so the job's folder is the upload's folder and the existing cleanup deletes it when the job ends;
- `snap.kg` is the bundle file itself (it holds `entities` and `triples` in MemPalace's schema, so `read_kg` reads it
  unchanged), and `snap.tunnels` is a `tunnels.json` the server extracts from the bundle into the job folder.

Survey, planning, dedup by legacy id (a rerun imports nothing new), the writer path, the report and the server's own
`RedactFilter` (idempotent on redacted text) are all reused as they are.

**Tenant mode** refuses an import whose request names a palace path, or none, unless it names an uploaded bundle (§2.2
item 7). A bundle id must be a ULID naming an existing `import-tmp/<id>/bundle.sqlite3`.

---

## 4. API additions

### 4.1 `client_project` on recall, context, remember and ingest

Added to `RecallRequest`, `ContextRequest`, `RememberRequest` and `IngestRequest` in `alice-core/src/api.rs`, with
`#[serde(default, skip_serializing_if = "Option::is_none")]` and `#[schemars(skip)]`, like `cwd`:

```rust
pub struct ClientProject {
    pub key: String,            // ProjectRef::key as detected on the client
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub remote: Option<String>,
    pub rules: ClientRules,
}

#[serde(tag = "state", rename_all = "snake_case")]
pub enum ClientRules {
    None,                         // the working tree has no .alice.toml
    File { text: String },        // its content, up to 64 KiB
    Unreadable { reason: String },
}
```

```json
"client_project": {
  "key": "github.com/icueth/alice", "slug": "alice", "name": "alice", "remote": "github.com/icueth/alice",
  "rules": { "state": "file", "text": "[privacy]\ncloud_denied = true\n" }
}
```

**Server validation** (`invalid` on failure; client strings are not echoed beyond the field name):
- `key`: 1–1024 bytes, no control characters, not `global`.
- `slug`: non-empty and equal to `slugify(slug)`.
- `name`: at most 200 characters.
- `remote`: at most 1024 bytes.
- `text`: at most 64 KiB.
- `reason`: at most 500 characters.

**Precedence in `Service::choose`:** explicit `project` slug, then `client_project`, then `cwd` (local mode only), then `global`.
The mirror is updated whenever `client_project` is present, even when an explicit slug picks the target. `choose` and
`rules_fail_closed` take one `Scope { project, cwd, client_project }` value, so the daemon's own spool drain (read-only database)
sees the identity too.

A local daemon (not in tenant mode) accepts `client_project` as well, with mirrors in its own home. This only happens when a
remote spool is drained after `alice remote unset`.

### 4.2 Ping capabilities

`PingResponse` gains `capabilities: Vec<String>` (`#[serde(default, skip_serializing_if = "Vec::is_empty")]`). This branch's
daemons answer `["client-project"]`, plus `"tenant"` under `--tenant`. Older clients ignore the field.

### 4.3 `retry_later` on refusals

`ErrorBody` gains `#[serde(skip_serializing_if = "is_false")] retry_later: bool`. It is set on a block-mode refusal while the
request's project rules are fail-closed (§3.5).

### 4.4 CLI

- `alice remote set <url>`, `alice remote status`, `alice remote unset` (§3.1).
- `alice daemon run --tenant` (§2.2).

Settings: none added. The environment variables `ALICE_REMOTE_URL` and `ALICE_REMOTE_TOKEN` join `NON_SETTING_ENV`.

### 4.5 Import bundle upload

`PUT /api/v1/import/bundle`. The body is the bundle file (`application/octet-stream`), and `Content-Length` is required.
The route is behind the token guard and has its own body rule in place of the 40 MB API limit. The server:
1. Refuses with `invalid` when an import is running or another upload is in progress, when the length is missing, zero, or
   above 4 GiB, or when the length would leave less than 2 GB free on the disk of `ALICE_HOME`
   (`backup::free_space`).
2. Sweeps `import-tmp/` folders holding an upload older than 1 hour that no job is using.
3. Streams the body to `import-tmp/<new ULID>/bundle.sqlite3.part` (folder 0700, file 0600). It stops and deletes the part
   when more bytes arrive than declared or the connection ends early.
4. Syncs the file and renames it to `bundle.sqlite3`.
5. Opens it read-only with `PRAGMA trusted_schema = OFF` and runs `quick_check`. It checks
   `bundle_meta.format = "alice-import-bundle"` and `version = 1`, and that the bundle's tables are present. Anything else
   deletes the folder and refuses with `invalid`.
6. Answers `201 {"bundle": "<id>", "bytes": n}`.

`ImportRequest` gains `#[serde(default)] bundle: Option<String>`. The bundle file's schema:
- `bundle_meta(key TEXT PRIMARY KEY, value TEXT)`: `format`, `version`, `source_backend`, `drawers`, `closets`,
  `held_back`, `notes` (a JSON array), `created_at`;
- `bundle_drawers(id TEXT PRIMARY KEY, text TEXT NOT NULL, wing TEXT NOT NULL, filed_at TEXT, meta TEXT NOT NULL)`, where
  `meta` is the redacted `DrawerMeta` as JSON;
- MemPalace's `entities` and `triples` tables;
- `bundle_tunnels(json TEXT)`.

---

## 5. Data flow

**Remember from an agent (remote).**
1. The agent calls `remember`. `alice mcp` takes cwd from the first root and finds the project (git, local).
2. It reads `.alice.toml` and redacts (block mode refuses here).
3. It sets an `idempotency_key` and drops `cwd`.
4. If the server is not verified yet, it sends `GET /u/ann/api/v1/ping?nonce=` and verifies the proof and capabilities.
5. It sends `POST /u/ann/api/v1/remember` with the Bearer token. Caddy strips the prefix and rewrites Host to `127.0.0.1:47801`.
6. The daemon checks the guard, validates `client_project`, updates the mirror, and redacts again (tenant rules plus the
   mirror).
7. The writer applies the write, and the judge worker later judges the memory. `cloud_allowed(slug)` reads the stored root, the
   mirror, so a `cloud_denied` project never reaches Jev.

**Precise recall (Jev on).** The client redacts the query (apply). The daemon's `query_cloud` (for the remote embedder and, for a
project not yet stored, for the grading call) and `DaemonJevPolicy` (for a stored one) use the mirror's rules. A denied project
gets the fast answer with the existing note.

**Offline write.** The ping fails within 3 s. The prepared request goes to the client spool, and the answer is "Queued: the
memory server … is unreachable; the memory is sent when it is back". The next `alice` call, or `alice mcp`'s 30 s replay, sends
it under the replay lock.

**Wrong tenant.** A client configured with `/u/bob` but holding Ann's token pings Bob's daemon. Bob's proof does not verify
against Ann's token, so the client stops with "did not prove it holds this token". No request carries the token, so there is
no 401.

**A tenant aims a setting at the LAN.** `PATCH /api/v1/settings` with `embedding.endpoint = "http://<nat-gateway>/"` is
refused by tenant-mode validation (§2.2, item 6). A public name that resolves to a LAN address passes validation, but the
daemon's connection is dropped by the unit's `IPAddressDeny` (R18), and the embedder reports the endpoint as unreachable.

---

## 6. Failure modes

| Failure | Behaviour |
|---|---|
| Server or network down | Reads fail within 3 s (the ping) with the URL and the reason. Writes are queued and replayed. Hooks stay bounded by that ping |
| One tenant's daemon down or restarting | Caddy answers 502, so it counts as unreachable (as above). systemd restarts it (`Restart=always`) |
| Token rotated by the admin | The proof fails, and the client says to run `alice remote set`. Writes are queued and sent after the new token is set |
| Server older than the client (no capability) | Refused before any token is sent; the message says to upgrade the server |
| Client older than the server | Older clients have no remote mode, so there is nothing to skew. The server ignores unknown fields |
| `.alice.toml` invalid, unreadable or blank on the client | Client: fail-closed redaction. Server: the mirror holds the same text or the marker, and `ProjectSettingsCache` fails closed |
| `.alice.toml` loosened or deleted | Applied on the server after three spaced looks (the existing gate). Until then the stronger rules stay |
| Mirror deleted or root missing on the server | Tenant mode fails closed (§2.2.4) until the next request recreates the mirror |
| Request with `cwd` but no `client_project` in tenant mode | `cwd` is ignored and the memory goes to `global`. Only a hand-made request can do this; the `alice` client always sends the identity |
| A client hammering with a bad token | Never sent by `alice` (proof first). Anything else: a 401 per request. No ban (the NAT hides addresses, §2.4); the 256-bit token is the protection |
| A tenant setting names a LAN, loopback or private URL, or a path outside its home | Refused by tenant-mode validation (§2.2, item 6). A DNS name that resolves to a LAN address is dropped by `IPAddressDeny` (R18) |
| Several client processes replaying at once | Advisory lock; the losers skip. Idempotency keys and content deduplication stop doubles |
| Caddy config invalid after a change | `caddy validate` fails, the change is undone, and the script exits non-zero. The running Caddy is never reloaded with a bad file |
| Upgrade breaks a tenant | `alice.prev` is restored, all tenants restart, and the script exits non-zero naming the tenant |
| Disk full on the server | The existing read-only and spool behaviour of the daemon applies (writes answer 202 "queued on the server") |
| Tenant backups or `remove` archives fill the disk | They share the server's 28 GB disk. `alice-server list` shows each home's size and the free space, and warns below 3 GB; off-machine copies are the admin's job (`docs/server.md`) |
| A bundle upload would leave less than 2 GB free, or is above 4 GiB | Refused before a byte is written; the client says how much is free and how large the bundle is |
| An upload breaks off (network, client killed) | The part file is deleted; nothing was started; the client can run the import again |
| An uploaded bundle is never imported | Swept after 1 hour by the next upload |
| The import job fails or the daemon stops mid-import | The existing M3 behaviour: the job's folder (the bundle) is deleted; a rerun uploads again and resumes by dedup |
| A drawer or fact holds a secret in block mode | Held back on the client and counted; it never leaves the machine |
| A tenant asks for a manual backup | Refused (§2.2 item 7) |

---

## 7. Security notes

- **Trust model.** The server's admin (root) can read every tenant's memory, token and Jev key. Each person's protection
  against the admin is what they keep out of Alice: client-side redaction and `cloud_denied`. Tenants are isolated from each
  other by Unix users, 0700 homes, separate tokens, separate ports bound to 127.0.0.1, and systemd hardening.
- **The token travels only:**
  - inside TLS;
  - after the ping proof;
  - never in a URL, log, error message or `Debug` output (`DaemonClient`'s `Debug` already omits it, and the new remote config
    type does the same);
  - never in Caddy's log (`log_credentials` stays unset).
- **Exposed surface per tenant:** `/api/v1/*` minus `/api/v1/admin/*`. The ping is public and returns the version, the instance
  and a proof. A proof is an HMAC-SHA256 under a 256-bit random key, so it does not help guess the token.
- **Path safety:** mirror directory names come from a hash, never from a client string, and the 64 KiB caps bound what a client
  can make the server write. Tenant settings that name a path must resolve inside `ALICE_HOME` (§2.2, item 6).
- **Egress (R18):** a tenant daemon reaches the public internet and localhost only. Caddy's admin API is on a Unix socket, so
  nothing on localhost can reconfigure the server.
- **Brute force:** 2^256 tokens. There is no per-IP banning, because the NAT hides client addresses (§2.4).
- **Backups:** tenant backups and `remove` archives sit on the same disk as the data. A copy off the machine is the admin's job.
- **Import:** a person's palace never leaves their machine unredacted. The bundle holds only redacted text, and in block mode
  the drawers with secrets are held back. The server reads an uploaded SQLite file read-only with `trusted_schema = OFF`
  after `quick_check`, as the tenant's own Unix user, so a crafted file can only affect the tenant that sent it.

---

## 8. Testing

All tests run with `env -u TYPESAFE_API_KEY -u ALICE_EMBEDDING_API_KEY`. Secret-shaped strings are built with `concat!`.

### 8.1 Unit and in-process integration (Rust)

A test **Caddy emulator**: an axum router that nests the daemon's router under `/u/<name>`, rewrites Host to
`127.0.0.1:<port>`, drops Origin and serves 404 for `/api/v1/admin/*`. With two in-process daemons behind it as tenants A and B,
the tests cover the following.

**Configuration (`remote.json` and environment):**
- URL rules: https accepted; http only to loopback; user info, query and fragment refused.
- The file is written 0600 and atomically, and made 0600 again on read.
- The environment overrides the file; one variable alone is an error.
- `Debug` and errors never contain the token.

**Connecting:**
- Remote mode never reads `daemon.json` and never runs the launcher. The test uses a launcher that records calls and a
  `daemon.json` pointing at an impostor.
- Wrong tenant: A's client on B's path fails, and B's recorder sees no `Authorization` header. The existing impostor cases
  (no proof, garbage, another token, another nonce) run in remote mode.
- A server without the `client-project` capability is refused before any token is sent.
- A server that answers its ping in 1 s connects in remote mode. The same delay locally counts as absent.

**Project identity:**
- `client_project` round trip: the project is created with the mirror as `root_path`, and the mirror holds the text.
- `none` removes the mirror file; `unreadable` fails closed with the marker message.
- In tenant mode `cwd` is ignored, and a missing root fails closed.

**Privacy:**
- `cloud_denied` sent by the client makes `DaemonJevPolicy::cloud_allowed` false for the judge worker's later call, and makes
  `query_cloud` deny both a stored project and one seen for the first time (the Task 22 path, with the mirror as its root).
- A loosened mirror waits for the gate; custom patterns from the mirror redact on the server.
- A secret in `remember` never reaches the server: a recording proxy checks the bodies with `grep -c` semantics.
- `block_secrets` refuses on the client and nothing is sent. Queries are redacted (apply).

**Spool:**
- With the server down, writes spool with `client_project` and no `cwd`; after the server comes back, the next command replays
  them in order.
- Two concurrent replayers produce no duplicates.
- A `retry_later` refusal is kept; a plain refusal is withheld.
- Local-mode entries are prepared at replay time.

**Tenant settings (§2.2, item 6):**
- Each row of the table: the refused values (`backup.dir` outside the home, after `..` and a symlink; `embedding.endpoint` and
  `judge.jev.endpoint` with `localhost`, 127.0.0.1, `[::1]`, 10.x, 172.16.x, 192.168.x, 169.254.x, 100.64.x, `fc00::`,
  `fe80::` and `http://`; `ollama`; `sync.remote`; `server.port`; `secrets.backend = "keychain"`) are refused on a patch and
  leave a `config.toml` load not applied, and an allowed value of each passes.
- The same values pass when the daemon is not in tenant mode.

**Import bundle (§3.8):**
- A bundle built from M3's synthetic fixture palace (`alice_core::import::fixture::synthetic_palace`, with a knowledge graph
  and tunnels from `build_kg` and `build_tunnels`) imports the same report counts (projects, memories, chunks, facts, links,
  skipped) as a local import of that palace.
- A drawer and a fact holding a secret-shaped string (built with `concat!`) never appear in the bundle's or the upload's
  bytes. In block mode they are held back and counted on the client; in redact mode the text arrives redacted.
- Ids with a secret map to `legacy_key` consistently in drawers, triples and tunnels.
- A rerun imports nothing new.
- The upload refuses: a missing or too large length; too little free space (an injected free-space function); a body
  longer than declared; a file that is not a bundle. Each of these leaves no folder behind.
- A tenant refuses `palace: Some(..)`, `palace: None` without a bundle, and an unknown bundle id.
- A tenant refuses `POST /api/v1/backups`.

**Commands and protocol:**
- Every refused command in §3.6 gives its message, and `remote status` exits 0 or 1 correctly.
- SSE: `/api/v1/events/stream` through the emulator delivers a `remember` event within 1 s.
- The ping, `ErrorBody.retry_later` and `capabilities` serialise as specified.

### 8.2 Admin script

- `shellcheck deploy/server/alice-server` is clean.
- `deploy/server/tests/run.sh` runs the script with `PATH` stubs for `useradd`, `userdel`, `systemctl`, `caddy`, `curl`, `ss`,
  `sudo` and `flock`, under a temporary root (`ALICE_SERVER_ROOT=<tmp>` prefixes every path the script touches). It checks:
  - `add` twice is idempotent;
  - port allocation skips used ports;
  - the snippet, unit and tenant file have the expected content and modes (the unit carries `MemoryMax=3G` and the R18
    `IPAddressAllow`/`IPAddressDeny` lines; the Caddyfile carries the admin socket), and no token is written outside the
    tenant home;
  - nothing fail2ban-related is installed;
  - the tenant file carries `ALICE_BACKUP_DAILY=2` and `ALICE_BACKUP_WEEKLY=0`;
  - `list` warns when the stubbed free space is under 3 GB;
  - `list` prints the free disk and the archive size;
  - `remove` archives before it deletes;
  - `upgrade` rolls back when a stubbed ping fails;
  - a bad name is refused.

### 8.3 Deployment task (the only time the server is touched)

1. Record `free -m`, `df -h`, `uname -m` and the Caddyfile in force.
   - Install rustup with the toolchain from `rust-toolchain.toml`, check out `feat/server-mode` and run
     `cargo build --release -p alice`.
   - Install the binary. Then remove `target/`, `~/.cargo/registry`, `~/.cargo/git`, the checkout and the toolchain
     (`rustup self uninstall -y`).
   - Record `df -h` again. This happens before any tenant or import exists.
2. Install `alice-server`, then run `alice-server add t1` and `alice-server add t2`. Diff the managed Caddyfile against
   `Caddyfile.pre-alice`: the IP certificate settings and `default_sni` must be unchanged. Check that a reload works through
   the admin socket and that nothing listens on port 2019. Record `ss -ltnp` and report any listener besides sshd, Caddy, the
   DNS stub and the tenants.
3. From this Mac, with a scratch `ALICE_HOME`:
   - `alice remote set https://<host>/u/t1` (token on stdin);
   - `alice remember` and `alice recall` in a scratch git repository that has a `.alice.toml` with `cloud_denied = true` and a
     custom pattern;
   - check the mirror on the server and `alice projects`;
   - `alice mcp` answers a scripted `tools/call` recall.
4. t1's token on `/u/t2`: the client refuses before sending the token, and a raw `curl` with that token gets a 401.
5. SSE: `curl -N` on `/u/t1/api/v1/events/stream` with the token shows an event within a second of a `remember`.
6. Check that the token does not appear in `/var/log/caddy/alice-access.log`, in `journalctl -u alice@t1`, or in the client's
   logs.
7. R18: t1's daemon has downloaded its model (public internet works). A `systemd-run --uid=alice-t1 -p IPAddressAllow=localhost
   -p IPAddressDeny=<the unit's ranges> curl -m 5 http://<nat-gateway>/` fails. Patching t1's `embedding.endpoint` to
   `http://<nat-gateway>/` is refused. Record one access-log line's `client_ip`, for the record (the gateway is expected).
8. Run `alice-server upgrade` with the same binary (a restart round trip) and `alice-server rotate t1`: the old token is
   refused, and `alice remote set` with the new one works.
8a. Import: a **synthetic** palace (built on this Mac by a small test binary with M3's fixture, a few thousand drawers
    with one `concat!`-built secret drawer), imported into t1 with `alice import mempalace --palace <it>`:
    - the counts equal a local import of the same palace;
    - the secret is not in the server's database (`grep -c` on a copy);
    - `import-tmp/` is empty afterwards;
    - `df -h` is recorded.
    Never a real person's palace.
9. `alice-server remove t1 --yes` and `remove t2 --yes`. Delete their archives too, to give the space back. Report the
   memory and disk use measured.
10. Only with the controller's go: `alice-server add` for `icue`, `joe-hermes`, `joe` and `petch`. Token handover is the
    controller's. Each person imports their own palace after handover.

### 8.4 Gates before the final review

`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` for the workspace (throttled per the lane
rules), plus §8.2. The documents: `docs/server.md` (EN; admin and person instructions) and the Thai section in `README.th.md`.

---

## 9. Rulings (what — why — cost if wrong)

| # | Ruling | Why | Cost if wrong |
|---|---|---|---|
| R1 | Project rules reach the server as a **mirror file** per project (`client-projects/<hash>/.alice.toml`), stored as the project's root | Every privacy path (`ProjectSettingsCache`, `Redactors`, `cloud_allowed`, `DaemonJevPolicy`, `query_cloud`, the judge worker) already reads `<root>/.alice.toml` with its fail-closed rules and loosening gate. A mirror reuses all of it with no second rules mechanism | If a later design wants rules in the database, a migration from mirror files to rows; small |
| R2 | Tenant mode is a **process flag** (`--tenant`), not a setting | A setting could be switched off through the API; the flag is fixed by the unit file | None for clients; a local daemon behaves as today |
| R3 | In tenant mode a **missing root fails closed** | A missing directory means rules this machine cannot know; "no rules" would send a denied project to the cloud | A restored laptop database stays fail-closed for a project until its client sends the identity once (intended) |
| R4 | Caddy exposes **only `/api/v1/*`**, not `/mcp` | Direct HTTP MCP from an agent would skip client-side detection and redaction, and its roots name paths the server cannot see, so memories would land in `global` | A person without the `alice` binary on their machine cannot connect an agent directly; adding the route later is one snippet line |
| R5 | `/api/v1/admin/*` is not routed; remote `alice token rotate` is refused; rotation is `alice-server rotate` | The rotation endpoint never returns the new token, so a remote rotation would lock the person out | A person cannot rotate their own token; they ask the admin (at most 5 people). A later endpoint could return the token once over TLS |
| R6 | The client redacts with `privacy.redact` **forced on** | Text that leaves the machine is redacted even for people who store raw text locally, the principle already used for the Jev judge | Someone who wants raw secrets on their server cannot store them; a later opt-out setting would fix it |
| R7 | The client redacts with its **local** global settings plus `.alice.toml`, not the tenant's settings; the server re-applies the tenant's | No extra round trip per write; the built-in detectors catch the common secrets; the server layer adds the tenant's custom patterns before storing | A tenant-only custom pattern is applied on the server, not before the network (still inside TLS) |
| R8 | The token is stored in `remote.json` (0600), not in the secrets store | The macOS keychain can prompt after each binary update, which would block hooks; a 0600 file matches how the daemon's own token is kept | A file readable by the person's other processes, as the local token is today |
| R9 | A verified remote client is **reused without a per-call ping**; it is re-verified after a transport error or 401 | A WAN ping per tool call doubles latency; TLS pins the host and the first proof pins the tenant | A route swapped by the admin mid-session is noticed at the next failure, not before the next call |
| R10 | The ping carries **capabilities**, and the client requires `client-project` | An older server would silently ignore `client_project` and file everything in `global` without the project's rules | One more field on the ping |
| R11 | Spool **replay by the client**: a background loop in `alice mcp` (30 s), and a 2 s budget after one-shot commands, under an advisory lock | No local daemon drains the spool in remote mode; hooks must stay fast | Writes queued by a one-shot command wait until the next `alice` call when no `alice mcp` runs |
| R12 | **Per-tenant model copy** (about 118 MB each), not a shared model directory | 0.6 GB for five tenants fits 19 GB free; sharing needs a new path setting and permission rules | 0.5 GB of disk |
| R13 | `alice-server` **owns the Caddyfile** from one template that reproduces the controller's IP-cert settings, with tenants as imported snippets | Editing a hand-written Caddyfile with sed is fragile; a template plus `caddy validate` is safe and idempotent | Other sites on this Caddy must go into an imported file; the backup `Caddyfile.pre-alice` keeps the original |
| R14 | Ports 47801–47899 on 127.0.0.1; tenant names `^[a-z][a-z0-9-]{0,15}$` | A fixed port per tenant is what Caddy routes to; the name goes into a user name, a URL path and a unit name | None at five tenants |
| R15 | `MemoryMax=3G` per tenant (controller's ruling) | One tenant's runaway must not starve the others; five times 3 GB fits the server's 15 GB, against a measured budget of 800 MB | A very large ingest by one person could be OOM-killed and restarted; raising the limit is one line |
| R16 | Build on the server (rustup and a native release build) in the deployment task | Avoids cross-compiling ONNX Runtime and glibc mismatches | Disk and time on the server (about 6 GB during the build); `target/` is removed afterwards if the disk is tight |
| R17 | The remote client honours **system proxy** variables for `https://` only | TLS hides the token from a CONNECT proxy; some networks need one | A misconfigured proxy shows up as "unreachable" with its error |
| R18 | **No LAN access from a tenant daemon** (controller's ruling, with two layers added here): the unit's `IPAddressAllow=localhost` and `IPAddressDeny=` of the private, link-local, CGNAT and ULA ranges; Caddy's admin API moved to a Unix socket; tenant-mode validation of every URL and path setting (§2.2, item 6) | A tenant controls URLs its daemon calls; the LAN holds the Proxmox host and the gateway; localhost must stay allowed (Caddy, DNS), which would otherwise leave Caddy's admin API on 2019 open to a tenant | A tenant cannot use a LAN or self-hosted embedding server (Ollama); none is planned on the server |
| R19 | **No fail2ban jail** (controller's ruling) | The NAT masquerades: every client arrives from `<nat-gateway>`, so a ban would lock everyone out, including the IP certificate's renewal | Brute force is unthrottled, which is irrelevant against a 256-bit token |
| R20 | **MemPalace import through a redacted bundle made on the client** and read on the server by a third `Backend` (controller's ruling; seam confirmed against M3's code: `Backend::Bundle` + `BundleReader`, a pre-made snapshot as the entry point) | The palace is on the person's machine; secrets must not leave it; M3's job then runs unchanged | Two readers to keep in step with MemPalace's format: the client's (unchanged M3 code) and the bundle's (ours, versioned) |
| R21 | The bundle is **one SQLite file** (drawers, MemPalace-schema graph, tunnels JSON, metadata) | One stream to upload and cap; `read_kg` reads the graph tables unchanged; paging by id needs an index, not memory | A format change needs a version bump (`bundle_meta.version`) |
| R22 | **Wings travel as redacted names; no `ClientProject` per wing** | M3's wing map is name-based and imported projects are placeholders; the first `client_project` of the same slug adopts the placeholder and brings its mirror, which is the local behaviour | An imported project's own `.alice.toml` applies only from that project's first remote request on, as locally |
| R23 | **Backups pinned to 2 scheduled, 0 weekly, no manual** on tenants (environment plus a tenant-mode refusal) | The disk is fixed (§2.7); manual backups are kept apart from the rotation | A person cannot make an extra copy on demand; the admin can |
| R24 | **Upload guard:** 4 GiB cap, refused below 2 GB free after the upload, one at a time, swept after 1 hour | The import must never be what fills the disk | A palace whose redacted text exceeds 4 GiB cannot be imported in one go; no such palace is known |
| R25 | After the build, **only the binary stays** (target, caches, checkout and toolchain removed) | About 6 GB back before any data exists | The next build reinstalls the toolchain (minutes) |

---

## 10. Questions decided by the controller (relayed to the owner, who may overrule)

- **Q1 — Trust model:** the admin (the owner) can read every tenant's memory and key; this is documented in `docs/server.md`.
- **Q2 — History:** importing each person's own MemPalace palace into their own tenant is **in scope** (addendum 1, §3.8).
  Moving an existing local Alice home stays out of scope (possible by hand: the admin restores a backup into the tenant
  home, and R3 keeps those projects fail-closed until the owner's client sends each project's identity once).
- **Tenants:** `icue`, `joe-hermes`, `joe` and `petch`, created after the controller's go (§8.3 step 10).
- **Q3 — Remote mode switches a whole machine**, or one `ALICE_HOME` through the environment, not single projects.
- **Q4 — Resources (measured by the controller):** 15 GB of RAM (13 GB free), 4 vCPU, and a 28 GB root filesystem with 19 GB
  free. Five tenants need about 0.6 GB for models, plus their data and backups. The NAT masquerades (R19).

---

## 11. Implementation outline (for the plan)

1. `alice-core`: `ClientProject` and `ClientRules`, the `client_project` fields, `PingResponse.capabilities`, the `parse_project`
   marker, and the `NON_SETTING_ENV` entries.
2. `alice-daemon` server side:
   - tenant mode (flag, ignore `cwd`, missing root fails closed, tenant settings validation of §2.2 item 6, the refusals of
     §2.2 item 7);
   - the mirror module (`client_projects.rs`);
   - the `Scope` value through `choose` and `rules_fail_closed`;
   - `retry_later`; the ping capabilities.
3. `alice-daemon` client side:
   - the `remote.json` and environment config module;
   - the remote target in `DaemonClient` (proof without `daemon.json`, capabilities, timeouts, errors, proxy);
   - request preparation (§3.3, §3.4);
   - the spool sink trait and remote replay with its lock.
4. `alice` CLI: `alice remote set/status/unset`, `daemon run --tenant`, the refusals of §3.6, the remote doctor checks, and the
   replay after one-shot commands.
5. `alice mcp`: the background replay and the remote flag timeout.
5a. Import (§3.8, §4.5):
    - `Backend::Bundle` with `BundleReader` and the bundle writer in `alice-core`;
    - the redacting bundle builder in `alice-daemon` (client side);
    - the upload endpoint and the bundle entry point of the import job;
    - `alice import mempalace` in remote mode.
6. The Caddy emulator test harness, and the end-to-end tests of §8.1.
7. `deploy/server/alice-server` with its stub tests (§8.2): the unit with R15 and R18, the Caddyfile with the admin socket,
   no fail2ban.
8. Documents: `docs/server.md`, the `README.th.md` Thai section, and a short pointer in `README.md`. `docs/server.md` covers:
   - the trust model (Q1);
   - why there is no per-IP ban, with the example filter (§2.4);
   - that tenant backups and `remove` archives share the 28 GB disk with the data, and off-machine copies are the admin's job.
9. Merge the head of `feat/m2-judge-settings` again before the final review (the M2 final-review fix wave lands after
   2026-10-16 17:00).
10. The deployment task (§8.3), only after the controller's go.
