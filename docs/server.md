# Running Alice on a shared server

This guide covers running Alice for a small team on one Linux server. It has three parts: what the setup is, what the admin does, and what a person does.

Placeholders used below: `<host>` is the server's public name or address, `<name>` is a person's tenant name, `<slug>` is a project name, and `<token>` is a person's secret token. Never paste a real token into a document, a chat or a command line.

## What it is

- **One daemon per person.** Each person (a "tenant") gets their own Alice daemon, their own Unix user, their own data directory and their own token. Nothing is shared between tenants by default.
- **One URL per person.** A person's endpoint is `https://<host>/u/<name>`. The MCP endpoint is `https://<host>/u/<name>/mcp`. Caddy terminates TLS and forwards each path to the right daemon on the loopback interface.
- **Direct MCP is the way in.** The team connects an agent (Claude Code, Codex and others) straight to the MCP endpoint, with the token in an `Authorization: Bearer` header. No client binary is needed.
- **Platforms.** The `alice` binary runs on macOS and Linux. On Windows, or anywhere the binary is not installed, connect an agent to the MCP endpoint. The server itself runs on Linux.
- **A shared team memory** is optional. See [Shared team memory (admin)](#shared-team-memory-admin) and [Shared team memory (person)](#shared-team-memory-person).

### Trust model

The server's admin can read every tenant's memory, token and Jev key. The admin owns the machine. What protects a person from the admin is what they choose not to put into Alice. Tenants cannot read each other: each has its own Unix user, a 0700 home directory, its own token and its own loopback port, and the service runs under systemd hardening.

A tenant cannot reach the admin's network. The service unit allows only the public internet and localhost, and tenant mode refuses settings that point at private or loopback addresses. A tenant also cannot change the settings the admin owns: the Jev endpoint, model and daily budget (pinned in its tenant file, because the Jev key is the owner's), its data directory, its port, its backup location and rotation, its secrets backend (always the private file), and the redaction switch (always on).

## For the admin

### Requirements

- A Linux server with systemd, root access, and `caddy` installed.
- An **x86-64 CPU with AVX** (AVX2 is not needed). The embedding runtime stops with "Illegal instruction" without it. `alice-server` checks for AVX before `add` and before `upgrade`, and refuses to go on if it is missing. Before it installs a binary, `upgrade` also has it embed a test text (see [upgrade](#upgrade)), so a runtime this CPU cannot run is never switched in.
- On Proxmox, set the VM's CPU type to `host`. Then shut the VM down and start it again. A reboot from inside the guest does not apply a CPU type change. Proxmox's default "QEMU Virtual CPU" has no AVX.
- Build the server binary on the server with `deploy/server/build-release` (below). A plain `cargo build` links an ONNX Runtime that needs AVX2.
- Speed: on 4 vCPUs of a Xeon E5-2620 (2012, AVX without AVX2) Alice embeds about 5.5 chunks a second, so importing a palace of 121,000 chunks takes about 6 hours. While an import embeds, the other tenants share the CPU with it.
- Memory: each tenant daemon is capped at 3 GB (`MemoryMax=3G`); the measured use is about 800 MB. Disk: see [Disk budget](#disk-budget).

### One-time setup

1. Install the script `deploy/server/alice-server` as `/usr/local/sbin/alice-server`.
2. As the admin user (not root), in a checkout of the repository, build the binary:

   ```
   deploy/server/build-release
   ```

   It downloads Microsoft's ONNX Runtime 1.24.4 for Linux x86-64 once (checked against a pinned SHA-256), builds `alice` linked to it, and leaves `target/server-dist/bin/alice` with the library in `target/server-dist/lib/alice`. Then install both:

   ```
   sudo alice-server upgrade target/server-dist/bin/alice
   ```

   `upgrade` is also the first install. It puts the binary in `/usr/local/bin/alice` and the library in `/usr/local/lib/alice`, where the binary finds it by itself.

3. Write the public host to `/etc/alice/server.env`:

   ```
   ALICE_PUBLIC_HOST=<host>
   ```

   The script reads the host from this file, so no address is written into the script. It stops with a message if the file or the value is missing.

4. Run `alice-server add <name>` for the first person. The first run sets up the base: directories, the systemd unit `alice@.service` and the Caddy configuration.

`alice-server` manages `/etc/caddy/Caddyfile` from one template. The first time it runs, it copies your existing Caddyfile to `/etc/caddy/Caddyfile.pre-alice` and never overwrites that copy. Compare the two files before the switch. After every change it runs `caddy validate`, then reloads Caddy. If validation fails, it undoes the change and exits with an error. The template also moves Caddy's admin API from `localhost:2019` to a Unix socket, so a tenant cannot reach it.

The script takes a lock, so two runs cannot overlap, and it is safe to run twice: a repeated command leaves the same state and says so. `import` is the exception: it can run for hours, and two may run at once. While an import runs, the admin's or a person's own upload over HTTPS, `upgrade`, `add --team`, `add --tz`, and `remove` or `adopt` of that tenant refuse to start, because they would restart or stop the daemon the import runs in. They ask each daemon (`alice import status`), so an upload, which never takes the script's lock, holds them back too.

### Commands

```
alice-server add <name> [--tz <IANA zone>]
alice-server add --team team
alice-server remove <name> [--yes]
alice-server rotate <name>
alice-server list
alice-server upgrade <binary>
alice-server import <name> <palace-dir> [--dry-run]
alice-server adopt <name> <backup-file>
alice-server jev-key set
alice-server jev-key status
alice-server cli <name> <alice arguments...>
```

A tenant name matches `^[a-z][a-z0-9-]{0,15}$` (a lowercase letter, then up to 15 lowercase letters, digits or hyphens).

#### add

`alice-server add <name>` creates the user, the home directory (`/srv/alice/<name>`, mode 0700), a tenant file in `/etc/alice/tenants/` and a port (the lowest free one in 47801-47899). It starts the daemon, waits for it to answer (the first start downloads the model, about 130 MB), adds the Caddy route and prints the details once:

```
Created tenant <name>.
URL:    https://<host>/u/<name>
MCP:    https://<host>/u/<name>/mcp
Token (shown once, hand it over now; alice-server rotate <name> gives a new one):
  <token>
Claude Code, with the token in the environment variable ALICE_TOKEN:
  claude mcp add --transport http alice 'https://<host>/u/<name>/mcp' --header 'Authorization: Bearer ${ALICE_TOKEN}'
The .mcp.json form is in docs/server.md.
```

The token is shown once. Hand it to the person over a private channel. If the tenant already exists, `add` prints its URL (not the token) and stops. If anything fails, it undoes what it created.

The `claude mcp add` line keeps `${ALICE_TOKEN}` literal, in single quotes, so the token never lands in a saved configuration. The recommended setup for a person's machine is [setup-server.md](setup-server.md) instead: it keeps the token in `~/.config/alice/token` and never in the environment, and connects Claude Code through a `headersHelper` script (see [For a person](#for-a-person)).

The tenant file holds no secret. It sets `ALICE_HOME`, `ALICE_SERVER_PORT`, `ALICE_LOG=info`, `ALICE_BACKUP_DAILY=2`, `ALICE_BACKUP_WEEKLY=0` and `TZ`, the Jev pins below, and `ALICE_CAPTURE_RAW_RETENTION=180d`: captured transcripts (SP2 capture) age out after 180 days, while imported history and memories stay. The environment wins over the person's settings, so they cannot keep transcripts longer. To change one tenant's, edit that line, then `systemctl restart alice@<name>`; `upgrade` adds the line to tenants made before it and keeps one the admin changed.

`TZ` is the person's time zone, `Asia/Bangkok` unless `--tz` names another IANA zone (it must exist under `/usr/share/zoneinfo`). The daemon runs in it, so an import reads MemPalace's local times with the offset of the person's own machine, as it would on their Mac; stored times stay UTC. Run `alice-server add <name> --tz <zone>` on an existing tenant to change its zone (it restarts that daemon). `upgrade` gives `TZ=Asia/Bangkok` to a tenant made before this line existed and keeps a zone set with `--tz`.

#### remove

`alice-server remove <name>` asks for confirmation (skip it with `--yes`), stops the daemon, removes the Caddy route, archives the home to `/srv/alice/backups/<name>-<UTC time>.tar.gz` (root only, mode 0600) and then deletes the user, the home and the tenant file. The archive stays until you delete it.

#### rotate

`alice-server rotate <name>` replaces the token and prints the new one once, as `add` does. The daemon refuses the old token at once. Hand the new token to the person. Their agents keep working as soon as `~/.config/alice/token` holds the new value: the Claude Code helper reads the file when it connects, and the Codex function when Codex starts. Capture pauses for an hour after the server first refuses the old token; running the capture setup line again ends the pause at once (it asks for the new token).

#### list

```
name             port   state      version    size     url
<name>           47801  active     0.1.0      1234 MB  https://<host>/u/<name>
team             47802  active     0.1.0      56 MB    (team, no URL)
free: 19.0 GB on /; remove archives: 0 MB
```

`list` shows one row per tenant, then the free space on the disk that holds `/srv/alice` and the size of the `remove` archives. Below 3 GB free it prints a warning. It never prints a token.

#### upgrade

`alice-server upgrade <binary>` checks that the new binary runs, then runs its self-test: `alice model check` in a throwaway home under `/tmp` loads the embedding model and embeds one text. The model is kept in `/var/cache/alice-server/models` (about 136 MB, downloaded the first time). If the self-test dies (an "Illegal instruction" is named as such), fails or takes longer than 5 minutes, nothing is installed. Otherwise it keeps the current binary as `/usr/local/bin/alice.prev` and the current ONNX Runtime as `/usr/local/lib/alice/libonnxruntime.so.1.prev`, installs the new ones (the library when the binary comes from `build-release`, from `../lib/alice` beside it), and self-tests the installed binary. Then it restarts every tenant and waits for each to answer. If the installed binary fails its self-test, or any tenant does not come back, it restores the previous binary and library, restarts everything again and exits with an error (naming the tenants that failed). It prints the free space before the restarts, because a schema migration copies each tenant's database once. On a server with no binary yet it is the first install.

#### import

A person who has no `alice` binary can still bring their MemPalace palace. They hand the palace folder to you (for example, you receive it with `scp`). Then:

```
alice-server import <name> <palace-dir> --dry-run
alice-server import <name> <palace-dir>
```

The script moves the folder into the tenant's `import-inbox/` (owned by the tenant, mode 0700), runs the importer as the tenant against its own daemon, and prints the report. It deletes the copy when the job ends, whether it worked or not.

- A **real import consumes `<palace-dir>`**: the folder is moved, not copied. Keep your own copy if you need one.
- A **dry run works on a copy** and keeps your folder. It reports what would be imported and changes nothing.
- The importer always redacts secrets. Drawers that contain secrets are held back when the tenant's privacy mode blocks them, and the report lists them.
- It refuses to start when twice the palace's size would leave less than 2 GB free.
- The tenant daemon accepts a palace only from inside its own `import-inbox/`.
- Import palaces one at a time, and only with the person's consent. The raw palace exists on the server only while the job runs.

#### Upload import (the admin's view)

A person can import their own MemPalace with `upload.sh` (see "A person imports their own MemPalace" below); you do nothing per person.

- **The route.** `POST /api/v1/import/upload` on the tenant's own daemon takes the gzip tar as the request body, behind the person's token. It streams the body to `import-inbox/upload-<id>.tar.gz` in the tenant's home (mode 0600), never holding it in memory, then unpacks, imports with the same code and redaction as `alice-server import`, and deletes the archive and the unpacked folder on every path. `GET /api/v1/import/status` shows the progress. `GET /api/v1/import/upload.sh` serves the script (public, static, no secret; Host and Origin checks still apply).
- **Caddy limits.** `alice-server add` writes the tenant's route with `request_body max_size 4GiB` and read and write timeouts of 30 minutes for the upload path only; every other path keeps Caddy's defaults. Tenants added before this was introduced have the old route: `alice-server upgrade` rewrites every personal tenant's route and reloads Caddy once, so run an upgrade (not just a restart) after installing a new version. A tenant whose limit you want higher or lower sets `import.max_upload_bytes` (also `ALICE_IMPORT_MAX_UPLOAD_BYTES`) in its `config.toml`; keep Caddy's `max_size` at or above it.
- **One import at a time** per tenant: a second upload (or an `alice-server import`) gets 409 until the running job ends.
- **Disk guard.** An upload is refused (and a running one stopped) when the free space would fall below about three times the archive's size plus 2 GB, the same 2 GB reserve as `alice-server import`. The unpacked size is also limited to 12 times the archive and never more than the upload limit; an archive with links, devices, absolute or `..` paths, or more than 50,000 entries is refused whole.
- **Cleanup.** Nothing of an upload stays after it ends. A daemon that is restarted removes any `import-inbox/upload-*` leftovers when it starts; folders you put there for `alice-server import` are never touched.

#### adopt

`alice-server adopt <name> <backup-file> [--cloud-denied <slug,slug...>]` makes a person's own Alice database, from `alice backup now` on their machine, the database of their tenant. Use it for someone who already runs Alice locally: their projects, cleanups and memories come along as they are, with every embedding already done. See [Moving your local Alice to the server](#moving-your-local-alice-to-the-server) for the person's side.

- The tenant must be fresh: `adopt` refuses one that holds any memory, chunk or fact.
- The file must be a regular file you own (copy it into your home with `scp`), and the disk needs twice its size plus 2 GB free.
- `adopt` stops the tenant, checks the file (SQLite's `integrity_check`, Alice's tables, and the same schema as the installed binary: an older one says "upgrade the source Alice first", a newer one "upgrade the server first"), keeps the tenant's own database as a before-restore backup in its backups folder, and puts the file in its place.
- It clears what names the person's machine: project folders (`root_path`), project keys made from a folder path, and transcript paths (a hash and the file name stay). Projects keep their slugs and aliases, so `?project=<slug>` finds them.
- It redacts every memory, raw chunk and history record again with the tenant's rules, which are always on, and drops the judge cache. A memory whose redacted text equals another one's is moved to the trash.
- Settings, secrets, the token and the Jev key stay the tenant's own: only the database is replaced. So do the person's cloud rules: the projects they kept from the cloud on their machine (their `privacy.cloud_denied_projects`, and repositories whose `.alice.toml` says `cloud_denied = true`) are not denied on the tenant unless you name them with `--cloud-denied`, which sets the tenant's list once it is up. The report's `projects_with_roots` lists the projects that had a working tree on the machine, the ones to ask about.
- The tenant starts again and rebuilds its vector index from the stored embeddings (no re-embedding, unless the source used another model). If it does not come back, its own database is put back.

It prints a report: memories, chunks, facts and projects, the paths cleared and the redactions. Facts, entity names and topics are taken as they are, not redacted again. The file is rewritten at the end (`VACUUM`), so nothing the redaction replaced or the person had deleted stays in it. Delete the backup file afterwards: it holds what the person's machine stored, before the server's redaction.

#### jev-key

On this server Jev is on for everyone, with the owner's one Jev key. People never see or supply a key.

```
sudo alice-server jev-key set        # paste the key at the prompt (it is not shown), or pipe it in
sudo alice-server jev-key status
```

`jev-key set` reads the key from the terminal with echo off, or from standard input; never from the command line or the environment. It keeps it in `/etc/alice/jev.key` (root's only, 0600) and gives it to every tenant, the team store included: into the tenant's own `secrets.toml` (0600, its user's), through its running daemon, with `judge.provider = jev` and `distill.provider = jev` (SP2: captured sessions are distilled into memories with the same key and budget). It takes effect at once; nothing restarts. `alice-server add` gives every new tenant the key. Run `jev-key set` again to rotate the key everywhere. `jev-key status` says whether a key is set and, for each tenant, whether it holds the server's key, another one or none. It never prints a key or part of one.

**The key stays the owner's.** Each tenant file pins `ALICE_JUDGE_JEV_ENDPOINT` (Jev's own endpoint) and `ALICE_JUDGE_DAILY_BUDGET_USD`, and the environment wins over a person's settings: a person cannot send the shared key anywhere else or raise the budget, and their settings show the key as set, without any part of it. `upgrade` adds both lines to tenants made before them.

**Budget.** Each tenant's judge budget is 1.00 USD a day by default. The deployment-wide ceiling is the number of tenants times that: with four people and the team store, 5.00 USD a day. To change one tenant's, edit `ALICE_JUDGE_DAILY_BUDGET_USD` in `/etc/alice/tenants/<name>.env`, then `systemctl restart alice@<name>`; `upgrade` keeps it. When a tenant reaches its budget, precise recall answers with the search order and says so.


#### cli

`alice-server cli <name> <alice arguments...>` runs the `alice` command as the tenant's Unix user against that tenant's own daemon. Use it for a person's settings, or for a key of their own:

```
alice-server cli <name> config get judge
alice-server cli <name> config set judge.provider jev
alice-server cli <name> config set-secret jev_api_key
```

`config set-secret jev_api_key` reads the key from standard input. Run it as shown and paste the key when it asks, or pipe it in from a file or a secret manager. The script refuses a secret on the command line, because a command line is visible to other users in the process list and in shell history. Never type the key as an argument.

Settings are TOML values or plain text, and they take effect at once. The daemon refuses a few settings in tenant mode (for example `privacy.redact = false`, or a private-network URL).

### Shared team memory (admin)

The shared team memory lives in one extra daemon named `team`:

```
alice-server add --team team
```

This creates the store, prints its token once, and links every existing personal tenant to it by copying the token into each tenant's home (mode 0600). A tenant added later is linked when it is created. The team store has **no URL of its own**: Caddy has no route to it, and only the personal daemons on the same machine reach it, over loopback. `alice-server rotate team` replaces its token in every personal home; no restart is needed. Manage the store with `alice-server cli team …`, which gives the admin full control over its memories. The team store has its own judge, cloud policy, backups and disk use.

### Privacy settings you control

- **The server always redacts.** Text arrives inside TLS and is redacted on the server, with the tenant's rules, before anything is stored or logged. The redaction switch is on for every tenant and cannot be turned off.
- **A repository's `.alice.toml` does not apply over direct MCP.** Nothing on the server reads the file, because the agent connects to the endpoint and the server never sees the repository.
- **Per-project cloud denial is a tenant setting.** To keep a project away from cloud services such as Jev, add its slug to the tenant's `privacy.cloud_denied_projects` list:

  ```
  alice-server cli <name> config set privacy.cloud_denied_projects '["secret-project"]'
  ```

  The value replaces the whole list, so include every slug you want denied.

### Disk budget

The disk is not grown for Alice, so the budget matters. Plan for these uses:

| Use | Size |
|---|---|
| A tenant's home | about 16.8 KB per drawer with its two backups (measured: about 2.0 GB for 121,000 drawers), plus its own copy of the model, about 130 MB |
| The server build | about 3 GB in the admin's checkout and toolchain (`target/` 2.1 GB, rustup 0.6 GB, cargo 0.3 GB); `cargo clean` in the checkout gives back 2.1 GB, and the next build then takes about 8 minutes instead of 3 |
| The self-test's model | about 136 MB in `/var/cache/alice-server/models` |
| The previous version | `alice.prev` and `libonnxruntime.so.1.prev`, about 52 MB |
| Scheduled backups | one copy of the database each; about 1-1.5 GB for a large tenant |
| Backups per tenant | 2 daily, no weekly (`ALICE_BACKUP_DAILY=2`, `ALICE_BACKUP_WEEKLY=0`); a tenant cannot make manual backups. They stay in the tenant's own `backups` folder: do not set `ALICE_BACKUP_DIR` in a tenant file (a directory outside the home fails that tenant's settings closed) |
| `remove` archives | a gzip of the removed home without its model, in `/srv/alice/backups` |
| Schema migrations | one extra copy of each database, kept 30 days |

Rough totals: a large tenant uses about 4.4 GB with its two backups. Three smaller tenants use about 1.1 GB each. If four large tenants share one 28 GB disk, they do not fit with a 3 GB reserve.

The guards act in this order:

1. `alice-server list` warns below 3 GB free.
2. Imports are refused when they would leave less than 2 GB free.
3. The backup job skips a copy it has no room for.
4. As the last line, the daemon goes read-only and spools writes.

**When free space is under 3 GB:**

- Delete old `remove` archives in `/srv/alice/backups` that you no longer need.
- Set `ALICE_BACKUP_DAILY=1` in that tenant's file under `/etc/alice/tenants/`, then restart the tenant: `systemctl restart alice@<name>`.
- Copy anything you want to keep off the machine first.

**Backups are on the same disk as the data.** A disk failure loses both. Copies off the machine (rsync, object storage, a snapshot of the VM) are the admin's job. Alice does not make them.

### No per-IP banning

`alice-server` installs no fail2ban jail. On the reference server every connection, including SSH, HTTPS and the certificate checks, reaches the VM from the NAT gateway. Banning an address would ban the gateway, lock out every user and stop certificate renewal. The protection is the 256-bit token, which cannot be guessed.

If your NAT keeps the real client address, you may add a jail by hand. This filter is an **example and is not installed**:

```ini
# /etc/fail2ban/filter.d/alice-caddy.conf (example; needs real client addresses in the log)
[Definition]
failregex = ^.*"client_ip":"<ADDR>".*"status":401\b
datepattern = "ts":{EPOCH}
```

Check one line of `/var/log/caddy/alice-access.log` first. If `client_ip` is your gateway, do not use it.

### What is exposed

For each tenant Caddy exposes `/api/v1/*` and `/mcp`, except `/api/v1/admin/*` (shutdown and token rotation), which is never routed. The access log shows the `Authorization` header as redacted, never the token. The ping endpoint is public and shows only the version and an identity proof.

## For a person

Your admin gives you two things: your tenant address, `https://<host>/u/<name>` (the MCP endpoint is that address with `/mcp`), and your token, usually as a file. The token is the key to your memory: keep it out of chats, commits, URLs and shell profiles.

**The easy way:** hand [setup-server.md](setup-server.md) to Claude Code or Codex. It installs the token and the address, connects both agents, uploads your MemPalace if you want, and sets up automatic capture, without ever showing the token. The rest of this section is what it sets up, for reference and for other agents.

### The token and the address

Both live in `~/.config/alice` (mode 0700): the token in `token` and the address in `url`, one line each, mode 0600. The MCP helpers below, the capture script and the MemPalace upload read them each time they need them. To write them yourself:

```bash
mkdir -p ~/.config/alice && chmod 700 ~/.config/alice
(umask 077; read -rs T && printf '%s\n' "$T" > ~/.config/alice/token)    # paste the token, press Enter
(umask 077; printf '%s\n' 'https://<host>/u/<name>' > ~/.config/alice/url)
```

Nothing exports the token into your shell, so an environment dump (`env`, a tool's error report) never shows it. Never print these files; check them with `test -s` or `grep -c`.

### Claude Code

Claude Code takes its headers from a `headersHelper` script, so the token never sits in Claude Code's configuration. [setup-server.md](setup-server.md#step-2-claude-code-only-if-claude-exists) (Step 2) writes the helper, `~/.local/bin/alice-mcp-headers`. It prints the `Authorization` header with the token from `~/.config/alice/token` and, inside a git repository, an `X-Alice-Project` header with the repository's folder name. Claude Code runs helpers from `~/.claude`, so the helper finds the session's folder through its parent process with `lsof`. Then the entry, for every project:

```bash
claude mcp add-json alice "{\"type\":\"http\",\"url\":\"$(head -n 1 ~/.config/alice/url)/mcp\",\"headersHelper\":\"$HOME/.local/bin/alice-mcp-headers\"}" -s user
```

**The `.mcp.json` form**, for one repository: the same entry in the repository's `.mcp.json`, with `?project=<slug>` on the URL to pin its project.

```json
{
  "mcpServers": {
    "alice": {
      "type": "http",
      "url": "https://<host>/u/<name>/mcp?project=<slug>",
      "headersHelper": "/home/<you>/.local/bin/alice-mcp-headers"
    }
  }
}
```

It holds no token. The helper is named by its absolute path on your machine, so commit the file only if everyone who uses the repository keeps the helper at that path. A `?project=` pin does not reach capture: see [Which project a captured session lands in](#which-project-a-captured-session-lands-in).

### Codex

Codex sends the token from an environment variable and the project from another:

```toml
[mcp_servers.alice]
url = "https://<host>/u/<name>/mcp"
bearer_token_env_var = "ALICE_TOKEN"
env_http_headers = { "X-Alice-Project" = "ALICE_PROJECT" }
```

`bearer_token_env_var` names the variable Codex reads the token from, and sends as `Authorization: Bearer`; `env_http_headers` sends `X-Alice-Project` from `ALICE_PROJECT`. Do not export either in your shell profile. [setup-server.md](setup-server.md#step-3-codex-only-if-codex-exists) (Step 3) adds a small `codex` shell function that sets both for Codex alone: the token read from `~/.config/alice/token` each time Codex starts, and the project from the git repository's folder name. Append the table to `~/.codex/config.toml` by hand, after a backup: `codex mcp add` rewrites the whole file.

### Other agents

Any client that speaks MCP over streamable HTTP works. Give it the MCP URL and the header `Authorization: Bearer <token>`, with the token read from `~/.config/alice/token` when it connects (a headers helper, a wrapper, a secrets manager), never typed into a saved configuration.

### Which project a memory goes to

Alice picks the project in this order:

1. the `project` argument of the tool call;
2. the `?project=<slug>` on the MCP URL (at most 100 characters);
3. the `X-Alice-Project` header (the same limits; UTF-8, or percent-encoded UTF-8);
4. the folder name of the first workspace root the agent offers;
5. `global`.

Claude Code sends no workspace roots over HTTP, so without one of the first three every session lands in `global`. The `alice-mcp-headers` helper sends the header from the session's git repository, and the Codex function does the same. A folder name that is not plain ASCII (a Thai name, say) is sent percent-encoded, because Node cannot send it as it is; the server decodes it. A header that is not UTF-8 after decoding fails the call, rather than putting the memory in another project.

A slug that matches no stored project makes a new, empty project of that name. Memories you write now and an imported palace with the same wing name land together.

### Privacy over direct MCP

- The server redacts secrets in everything you send, whatever your settings say. Redaction cannot be turned off.
- **Your repository's `.alice.toml` does not apply.** The server never sees your repository. To keep a project away from cloud services such as Jev, ask your admin to add it to your `privacy.cloud_denied_projects`.
- The admin can read your memory. Do not store what you would not show the admin.

### Shared team memory (person)

If your admin created the team store, you can save a memory for the whole team. Tell your agent:

> Save this to the shared memory.

In Thai: "บันทึกอันนี้ลงความจำส่วนกลางด้วย". The agent then writes the memory to the team store only; it does not keep a personal copy.

- **Every team member can read it.** Do not share anything private.
- **Only the author or the admin can change it.** You can edit, archive, forget or undo only the shared memories you wrote. Anyone else gets "only the author or the admin can change a shared memory".
- **Recall includes it.** `recall` and `context` also search the team memory for the same project and the team's global memories. Team hits show their author and have ids that start with `team:`.
- **If the team store is down**, a shared write fails with a clear message and nothing is saved. A recall returns your personal results with a note that the team memory did not answer in time.
- **The team store has no URL of its own.** You never connect to it; your own daemon does.

How team memories meet your model judge: in precise recall, a team memory is graded by your model judge (for example Jev) only when **both** the team store's cloud policy for that memory's project and your own cloud policy for that project's slug allow it. If either one denies it, the memory is never sent to your judge. It appears after the graded results, in score order.

### Settings and your Jev key

On this server the admin gives every tenant the owner's Jev key (`alice-server jev-key set`), so precise recall uses the Jev judge without anything from you. If you ever need a key of your own, the admin runs, on the server:

```
alice-server cli <name> config set-secret jev_api_key     # the admin pastes your key at the prompt
alice-server cli <name> config set judge.provider jev
```

Send such a key over a private channel, never in a chat room or an issue. Without any key, the judge stays on the offline rules.

### Moving your local Alice to the server

If you already run Alice on your Mac, your admin can move your database to your tenant instead of importing MemPalace again: your project cleanups and the memories you wrote to Alice directly come along, and nothing has to be embedded again.

1. On your Mac, list the projects you keep from the cloud: `alice config get privacy.cloud_denied_projects`, and any repository whose `.alice.toml` says `cloud_denied = true`. Give that list to your admin with the file: the server does not read your repositories, so it applies only what you name.
2. Stop the agents that write to Alice, then run `alice backup now`. It prints the file it wrote, under `~/.alice/backups/`.
3. Copy that file to the server, to your admin's home, with `scp`.
4. Your admin runs `alice-server adopt <name> <file> --cloud-denied <your list>` on a fresh tenant and checks the report with you.
5. Point your agents at your tenant's MCP URL (see above) and stop writing to the local Alice. Two stores that both take writes drift apart, and nothing merges them back. Keep the local home as a read-only fallback.

The server clears your machine's folder paths from the projects and transcripts, and redacts everything again with its rules. Your Jev key is set on the tenant separately.

### A person imports their own MemPalace

You can upload your MemPalace from your own machine; your admin does not need your files. You need `curl`, `tar` and `gzip` (every Mac and Linux has them) and your token. `sqlite3` is recommended: with it the script takes a consistent copy of your databases while MemPalace is running. Without it, the script warns you and copies the files as they are, so MemPalace and its agents must be idle.

1. Save your token where the script looks for it, readable only by you:

   ```
   mkdir -p ~/.config/alice
   (umask 077; read -rs T && printf '%s\n' "$T" > ~/.config/alice/token)    # paste the token, press Enter
   ```

   The script refuses a token file that other people can read (`chmod 600 ~/.config/alice/token`) or that belongs to another user. The token goes to `curl` on its standard input; it is never put in a command line, a URL or the output.

2. Download the script from your own tenant (it holds no secret and no address):

   ```
   curl -fsS https://alice.example/u/<name>/api/v1/import/upload.sh -o upload.sh
   ```

3. Run it with your tenant's address (the MCP URL without `/mcp`):

   ```
   sh upload.sh https://alice.example/u/<name>
   ```

It finds your palace itself (`palace_path` in `~/.mempalace/config.json`, else `~/.mempalace/palace`) and your knowledge graph (`~/.mempalace/knowledge_graph.sqlite3`). If yours are elsewhere, add `--palace <folder>` and `--kg <file>`.

**What it does.** It copies a snapshot of the palace into a private temporary folder (mode 0700; symbolic links are skipped and nothing in your palace is changed), packs it into one `.tar.gz`, uploads it to your tenant, and waits while the server unpacks and imports it. It removes its temporary files when it ends, however it ends, including Ctrl-C. It needs about 1.2 times the palace's size free in `$TMPDIR` (default `/tmp`); set `TMPDIR` to a bigger disk if it says there is not enough.

**How long it takes.** The upload depends on your connection. The import on the server can take a good while more for a big palace (minutes to hours; the progress lines show how far it is). Vector search for the imported text is filled in afterwards, in the background.

**Seeing progress.** The script prints the server's state about every 10 seconds (receiving, unpacking, importing, and drawers done out of the total), and at the end a report in plain words: the drawers read, the memories and raw chunks added per tier, the memories per project, what was skipped and why, what was held back because it held a secret, and how many pieces of text still wait for their search vector. Its exit code is 0 only when the import finished. If your terminal closes during the import, the import goes on at the server; run the script again later and it tells you if one is still running, or ask your admin.

**Limits.** The upload may be up to 4 GiB (the admin can change this), and what it unpacks to may not be more than 12 times its size or more than that limit. Only one import runs at a time for your tenant; a second attempt is refused until the first ends. Importing again is safe: drawers already imported are recognized and not added twice.

**What is redacted.** The server redacts every text again with its own rules, exactly as `alice import mempalace` does: API keys, tokens, passwords and private keys are removed or, if your privacy mode blocks them, the drawer is held back. The report lists the held-back ones by kind of secret. The uploaded archive is deleted from the server as soon as it is unpacked, and the unpacked copy as soon as the import ends.

If you would rather not run anything, you can still give your palace folder to your admin, who runs `alice-server import` (see above).

## Capture for team members

A person's Claude Code and Codex sessions can be saved to their own tenant as they happen: every finished turn goes into the tenant's raw tier, in the session's project, where `recall` finds it with `include_raw`. Where distillation is on (`alice-server jev-key set` turns it on, see [below](#distillation-and-the-shared-jev-key)), ended sessions are also distilled into a few short memories by Jev. The person installs no Alice binary: capture needs `sh`, `curl` and the agent. What is kept and what never is, and how distillation works, are in [capture.md](capture.md).

### The setup line

The admin gives each person one line, to run on their own machine:

```
curl -fsSL https://<host>/u/<name>/api/v1/capture/setup.sh | sh -s -- https://<host>/u/<name>
```

`alice-server` does not print this line: write it from the person's tenant address, the URL `add` printed, which appears twice in it.

- **Run it after the MCP setup.** [setup-server.md](setup-server.md) (which an agent can run for the person) writes `~/.config/alice/token` and `~/.config/alice/url` first; the setup line then asks nothing. Without those files it asks for the token once, on the terminal with typing hidden, and writes both files (0600, in a 0700 `~/.config/alice`). It never takes the token from the command line or the environment, and never keeps a second copy.
- **Nothing is written before the token is checked.** The script checks the address (`https://`, or `http://` to `127.0.0.1` or `localhost` for tests) and asks `GET /api/v1/capture/status` whether the token works. A refused token from the file is reported, and the script asks once for a new one. Pointed at a local daemon instead of a tenant, it stops and says so.
- **Updates.** The tenant hook script updates itself from your Alice server once a day, over https, and updates the plugin's `hooks.json` when that changes. Re-run the setup line only to repair an install, and once after this release. So after an `alice-server upgrade` that changes the capture scripts, nobody has to do anything: each machine picks the new script up within a day, at its next agent event. A re-run of the line updates everything in place, keeps the person's `briefing=off` choice, and reuses `~/.config/alice/token` and `~/.config/alice/url` without asking.
- **Hooks load in new sessions.** The person starts a new Claude Code (or Codex) session afterwards.

`GET /api/v1/capture/setup.sh` and `GET /api/v1/capture/hook.sh` are public, like `/api/v1/ping`: static text compiled into the binary, with no secret and no address, behind the Host and Origin checks but no token. The hook script is served with its version line filled in: the first 12 hex digits of the SHA-256 of the script as built into the server, before that line is filled in, so a machine sees a new version exactly when the script changed.

### What it installs

All under `~/.config/alice` (mode 0700):

| File | Mode | What it is |
|---|---|---|
| `token`, `url` | 0600 | The person's token and tenant address, shared with their MCP setup. Read, never changed, when they exist |
| `capture.conf` | 0600 | One option, `briefing=on` or `briefing=off`; no secret |
| `alice-capture.sh` | 0700 | The hook script, downloaded from the tenant; it replaces itself with the tenant's newer version, at most once a day |
| `update-checked` | 0600 | When the last update check started (setup writes it too, so the first check comes a day after setup) |
| `state/` | 0700 | One `<agent>-<session>[-<agent_id>].offset` file per transcript (how far it was sent), a lock while one is being sent, and `update.lock/` while an update check runs |
| `capture.log` | 0600 | At most 256 KB: session ids, offsets, sizes, HTTP statuses. Never the token, never transcript text |
| `paused-until` | 0600 | Present for an hour after the server refused the token |
| `claude-marketplace/` | 0700 | A local Claude Code plugin marketplace, `alice-local`, holding the plugin `alice-capture` at the tenant's version |
| `codex-hooks.written` | 0600 | A copy of the Codex `hooks.json` setup wrote, so a re-run or an uninstall never touches a file the person edited |

**Claude Code.** Setup runs `claude plugin marketplace add ~/.config/alice/claude-marketplace` (or `marketplace update alice-local` on a re-run) and `claude plugin install alice-capture@alice-local --scope user`. Claude Code then merges the plugin's hooks itself. When the `claude` command is missing or fails, setup prints the two commands to type inside Claude Code instead: `/plugin marketplace add "<that folder>"` and `/plugin install alice-capture@alice-local`. The plugin runs this on `SessionStart`, `Stop`, `SubagentStop`, `PreCompact` and `SessionEnd`, with a 10-second timeout:

```
sh "$HOME/.config/alice/alice-capture.sh" hook claude-code || true
```

`|| true` keeps a missing script (after an uninstall, say) from ever failing an agent's turn.

**Codex.** When `codex` is installed, setup writes `$CODEX_HOME/hooks.json` (default `~/.codex/hooks.json`) with the same five events and `hook codex` (`SessionEnd` gets 3 seconds, Codex's limit). It writes the file only when it does not exist, or is still exactly the one setup wrote before. When the person has a `hooks.json` of their own, or `[hooks]` tables in `config.toml`, it changes nothing and prints the entries to add. Either way, **Codex runs the hooks only after the person trusts them: open Codex, type `/hooks`, and trust the Alice entries.**

### How it works

- **The hook returns at once.** It reads the agent's JSON on standard input, starts a background copy of itself with the session's details (no token among them), and exits 0. Only `SessionStart` waits, at most 2 seconds, for the briefing.
- **The background copy sends the new bytes.** It waits 2 seconds after a stop-type event (the transcript file can lag), takes a lock for that transcript, and posts the bytes after the last offset to `POST /api/v1/capture/transcript`. Bodies are at most 2 MiB, growing to 8 MiB when a turn is larger. The server keeps the cursor, stores only complete turns, and answers with the next offset; it never reads a path on the person's machine.
- **Nothing is lost when the server is away.** On a network error or a server error the offset stays, and the next event of that session tries again: the transcript file itself is the queue. Each `SessionStart` also finishes up to five other sessions whose transcripts grew meanwhile.
- **Nothing is stored twice.** A script that lost its offset is told the server's cursor (`409`) and jumps there; a turn sent again is skipped by its key.
- **A refused token pauses capture for an hour.** The log says "the token was refused; run the setup command again (capture is paused for an hour)". Running the setup line again ends the pause at once: it checks the token (asking for the new one when the stored one is refused) and clears `paused-until`. A new token written into `~/.config/alice/token` alone is used only when the hour is over.
- **It updates itself once a day.** The first background copy a day or more after the last check fetches `GET /api/v1/capture/hook.sh` over https (no token is sent), before it uploads anything. When the version differs and the download passes its checks (it starts with `#!/bin/sh`, holds the marker and version lines, ends with `main "$@"`, passes `sh -n`, and `dash -n` where dash exists, and a trial run prints its hooks file within 20 seconds), it replaces the script in one step; otherwise the installed script stays byte for byte. Then the plugin's `hooks.json`, and Codex's `hooks.json` while it is still the one setup wrote, follow the new script when they differ: Claude Code reloads the plugin with `claude plugin marketplace update` and then `claude plugin update`, in the background, each limited to 120 seconds, and Codex runs changed hooks only after the person trusts them again in `/hooks`. `capture.log` shows `capture hook updated <old> → <new>` for an update, `capture hook update refused (HTTP <code>…); the installed script stays` for a refusal (a download that fails a check, or a 2xx or 3xx answer other than 200), and `capture hook update check failed (curl exit <n>, …)` for a server it could not reach, or one that answered with an HTTP error; the next check is a day later either way. A tenant address on plain `http://` (only `127.0.0.1` or `localhost`, for tests) never updates: `capture hook update skipped: it runs over https:// only, …`.
- **The token** reaches `curl` only as a config line on its standard input (`curl -q -K -`, written by the shell's `printf`). It is never in a command line, a URL, a child's environment or a log.

### The briefing

When a Claude Code session starts (`source` `startup`, `clear` or `compact`; not `resume` or `fork`, which already hold one), the hook asks `GET /api/v1/capture/briefing` for the project's briefing and prints it as plain text, which Claude Code adds to the session. It is the same briefing as the `context` tool's, at most 10,000 characters.

- **It is bounded:** `curl --connect-timeout 1 --max-time 2`. A slow or unreachable server costs at most 2 seconds at the start of a session, and then nothing is printed; the MCP instructions still ask the agent for `context`. Any answer but `200 text/plain` prints nothing. The hook always exits 0.
- **Two switches turn it off:** the line `briefing=off` in `~/.config/alice/capture.conf` (no request is made), or the tenant setting `capture.briefing = false` (`alice-server cli <name> config set capture.briefing false`), which makes the server answer `204`.
- **Codex** gets no briefing from the hook; its MCP instructions cover it.

### Privacy on this path

It matches direct MCP:

- The text arrives inside TLS, unredacted, and the tenant redacts it with its own rules (always on) before anything is stored or logged.
- **A repository's `.alice.toml` is not mirrored.** The script reads two lines of it, at the git top level (else the session's folder), on the person's machine: `no_capture = true` (nothing of that session is sent, and the script's offset moves past those turns, so they are never sent later either) and `cloud_denied = true` (sent with `no_distill=1`: stored, never distilled). Only plain `key = true` lines are seen, not other TOML forms. A `.alice.toml` the script cannot read skips the event and keeps the offset (fail closed).
- **The tenant's settings decide the rest:** `capture.excluded_projects` (never captured) and `privacy.cloud_denied_projects` (never sent to Jev), set with `alice-server cli <name> config set …`.
- **Custom patterns and block mode from a repository's `.alice.toml` do not apply here.** They wait for a client that reads the repository on the person's machine (the deferred `alice remote`). The tenant's own `privacy.custom_patterns` apply.

### Which project a captured session lands in

The script names the project after the folder of the git repository the session runs in (not `$HOME`; outside git it names none, and the session goes to `global`). It sends that name as the `X-Alice-Project` header, percent-encoded so any folder name travels, and the tenant resolves it as it resolves the header over MCP: an existing slug or alias, else a new project of that name. The `alice-mcp-headers` helper and the Codex wrapper of [setup-server.md](setup-server.md) send the same folder name, so a repository's memories and its captured sessions land together.

**An MCP `?project=` pin does not reach capture.** If a repository's `.mcp.json` pins `?project=web` while its folder is `acme-web`, memories go to `web` and transcripts to `acme-web`. Pin the folder's name instead, or make one project a name of the other: `alice-server cli <name> projects merge acme-web web --yes` moves everything of `acme-web` into `web` and makes `acme-web` an alias, so later captures land in `web` too.

### Retention

Captured transcripts are kept **180 days** on every tenant (the open-source default is `forever`), pinned by the line `ALICE_CAPTURE_RAW_RETENTION=180d` in `/etc/alice/tenants/<name>.env`. `alice-server add` writes the line, and `upgrade` adds it to tenants made before it (saying `captured transcripts kept 180d for: <names>`) and keeps a value the admin changed. The environment wins over the tenant's `config.toml`, so a person's own settings cannot keep them longer; the admin changes the line (`90d` for the heaviest tenant, say) and runs `systemctl restart alice@<name>`. The daily job `capture_retention` deletes only captured chunks: imported MemPalace history and every memory stay, distilled ones included.

As an estimate, carrying over the 16.8 KB per imported drawer (with its two backups) that the [Disk budget](#disk-budget) measures to captured chunks, a heavy user (about 1,000 chunks a day) holds about 3.0 GB of captured transcripts at 180 days, and a lighter one (300 a day) about 0.9 GB. Captured chunks have not been measured yet.

### Distillation and the shared Jev key

Distillation is off by default (`distill.provider = off`), on a tenant as anywhere: the admin turns it on. `alice-server jev-key set` gives every tenant the owner's Jev key with `judge.provider = jev` and `distill.provider = jev` (see [jev-key](#jev-key)), and `alice-server add` does the same for a tenant added once the key is set. Without a server key, distillation stays off. To keep one tenant's off, run `alice-server cli <name> config set distill.provider off`, again after every `jev-key set`, which turns it back on.

- A person never sees or supplies the key. The setup line's summary says whether distillation is on (`distillation: on`), from the tenant's own setting.
- **Spend is per tenant**, inside its judge budget (`ALICE_JUDGE_DAILY_BUDGET_USD`, 1.00 USD a day by default), of which distillation may use half (`distill.budget_share = 0.5`). **The deployment-wide ceiling** is the number of daemons times that budget: 5.00 USD a day with four people and the team store, of which distillation can be at most 4 × 0.50 = 2.00 USD (the team store captures nothing). Expected spend is far lower, about 0.001-0.003 USD per distilled session.
- A tenant's `privacy.cloud_denied_projects` are never sent to Jev; their sessions are captured but not distilled.
- When distillation is first switched on, the sessions captured in the last 7 days are distilled, newest first; older ones only on request (`alice-server cli <name> capture distill <session-id>`).

### Checking and removing

```
sh ~/.config/alice/alice-capture.sh status
sh ~/.config/alice/alice-capture.sh uninstall
```

`status` shows the address, whether the server accepts the token, whether capture and distillation are on, the hook script's version and how long ago it last checked for an update, the briefing switch, how many transcripts are followed, the plugin and the Codex hooks. It never prints the token. It also lists transcripts that wait:

- `stopped: <key>`: the server's cursor is past the file's end (the server already holds more of that session than the file has, for example after the file was replaced). Delete `~/.config/alice/state/<key>.offset` to start that transcript again.
- `waiting: <key>`: the server took nothing at that size (no complete turn yet, or the tenant's rules for that project are not known right now); it is sent again when the file grows. Deleting the state file resets it.

`uninstall` removes the plugin and the marketplace (or prints `/plugin uninstall alice-capture@alice-local` and `/plugin marketplace remove alice-local` to type in Claude Code), removes the Codex `hooks.json` only if it is still exactly the one setup wrote, and then asks before deleting the script, `state/`, `capture.conf`, `capture.log`, `paused-until` and `update-checked`. It never deletes `token` or `url`, which the MCP setup uses. Restart open Claude Code and Codex sessions afterwards, so none still runs the removed hooks.

### What the admin's script provides

What capture takes from `alice-server` (SP2 spec §9.5):

| Interface | What `alice-server` does |
|---|---|
| The setup line | Nothing: it does not print the line. Write it from the tenant's URL (see [The setup line](#the-setup-line)) |
| Capture retention | `add` writes `ALICE_CAPTURE_RAW_RETENTION=180d` into the tenant's file; `upgrade` adds it where it is missing and keeps a value the admin changed |
| Distillation with the shared key | `jev-key set` gives every tenant the key with `judge.provider = jev` and `distill.provider = jev`; `add` does the same for a new tenant once the key is set |
| Adoption | `adopt` moves a database whole, so its capture cursors and turn keys travel with it; it clears the captured sessions' transcript paths and file identities, which a tenant never reads |

### Moving a local Alice that already captured

If the person ran capture on their machine before you adopt their database (see [adopt](#adopt)):

1. On their machine, they let capture finish (end the open session, or wait a few minutes for the daemon's sweep), and run `alice import mempalace` one last time if MemPalace is still active.
2. You adopt the database. Its capture cursors and turn keys travel with it.
3. On their machine, they run `alice hooks uninstall claude` (and `codex`), then the setup line instead.

The script's first upload of a session the machine already captured starts at offset 0; the tenant answers with its cursor (`409`), the script jumps there, and a turn sent again is skipped by its key. Nothing is stored twice. Details: [capture.md](capture.md#when-you-move-to-a-server-tenant).

### Troubleshooting capture

| What you see | Meaning | What to do |
|---|---|---|
| Setup prints `/plugin marketplace add …` and `/plugin install alice-capture@alice-local` | The `claude` command is missing or could not install the plugin | Type the two commands inside Claude Code |
| `… is a local Alice daemon, not a server tenant` | The address points at a local daemon | Use the tenant address `https://<host>/u/<name>`; a local install uses `alice hooks install` |
| `the server refused that token too … Nothing was changed.` | The token is wrong or was rotated | Ask the admin for the current token |
| `there is no terminal to ask for the token on` | Setup ran without a terminal and without a token file | Put the token in `~/.config/alice/token` (one line, 0600) and run the line again |
| `capture.log`: "the token was refused; run the setup command again (capture is paused for an hour)" | `alice-server rotate` replaced the token | Run the setup line again: it asks for the new token and ends the pause. Writing the token file alone waits out the hour |
| `status` says `server: unreachable` | No connection, or the tenant is down | Nothing is lost; capture resumes at the next event |
| `capture.log`: "… holds a JSON escape this script does not decode (such as \u); event skipped" | A path or folder name reached the hook in an escaped form | That event is skipped; tell the admin if it repeats |
| Codex sessions are not captured | The hooks are not trusted yet, or the person has their own `hooks.json` | Trust them in Codex's `/hooks`; add the entries setup printed |
| No briefing at session start | `briefing=off`, `capture.briefing = false`, a resumed session, or a server slower than 2 s | Check `status`; the agent still calls `context` itself |
| Memories and transcripts of one repository in two projects | An MCP `?project=` pin differs from the folder name | See [Which project a captured session lands in](#which-project-a-captured-session-lands-in) |
| `capture.log`: "capture hook update refused …" or "capture hook update check failed …" | The day's update check got no usable script; the installed one stays | Nothing: it tries again a day later. If it repeats, run the setup line again |
| `capture.log`: "the plugin's hooks.json changed, and claude plugin update failed …" | An update changed the plugin's hooks, and the `claude` command did not reload them | Run the setup line again |
| `capture.log`: "updated … hooks.json with the hook script; Codex runs the changed hooks only after you trust them again …" | An update changed the Codex hooks | Open Codex, type `/hooks`, and trust the Alice entries again |

## Troubleshooting

| What you see | Meaning | What to do |
|---|---|---|
| `alice-server: this CPU lacks AVX` | The VM's CPU type has no AVX | Set the CPU type to `host`, shut the VM down, start it again |
| `the new binary failed its self-test (killed by SIGILL: …)` | This binary's ONNX Runtime needs instructions the CPU lacks; nothing was installed | Build it with `deploy/server/build-release` on the server; a plain `cargo build` needs AVX2 |
| `the new binary failed its self-test (exit …; last output: …)` | It could not load or run the model (often no network for the first model download) | Read the last output; check that the server reaches huggingface.co, then run `upgrade` again |
| `write ALICE_PUBLIC_HOST=<the server's public IP or name> to /etc/alice/server.env first` | `/etc/alice/server.env` does not exist | Create it with `ALICE_PUBLIC_HOST=<host>` |
| `ALICE_PUBLIC_HOST is empty in /etc/alice/server.env` | The file exists but the host is empty | Write `ALICE_PUBLIC_HOST=<host>` to it |
| HTTP 401 from the MCP URL | The token is wrong or was rotated | Check that `~/.config/alice/token` holds the current token (the check in [setup-server.md](setup-server.md) Step 1 prints `200`); ask the admin for a new token |
| HTTP 404 from the MCP URL | The tenant name is wrong or was removed; `/u/team/` never exists | Check the URL with the admin |
| The agent cannot connect to the server entry | Claude Code: the helper `~/.local/bin/alice-mcp-headers` is missing, not executable, or cannot read `~/.config/alice/token`. Codex: it was not started through the `codex` shell function, so it has no token | Run Step 2 or Step 3 of [setup-server.md](setup-server.md) again; start Codex from a new terminal |
| "memory daemon is starting (downloading the embedding model …)" | A new tenant is fetching its model | Wait a few minutes and try again |
| A shared write fails | No team store, or it did not answer | Ask the admin whether `alice-server add --team team` was run, and check `alice-server list` |
| "only the author or the admin can change a shared memory" | The memory was written by someone else | Ask the author, or the admin |
| "the team memory did not answer in time; this answer is personal only" | The team store was slow or down | Try again; tell the admin if it repeats |
| `alice-server list` warns about free space | Under 3 GB free | See [Disk budget](#disk-budget) |
| A tenant is not `active` in `list` | The daemon stopped | `journalctl -u alice@<name>`, then `systemctl restart alice@<name>` |
| `alice-server upgrade` exits with tenant names | Those tenants did not restart | The previous binary was restored; read `journalctl -u alice@<name>` |
| `… still down: <names> … cannot start on the old one` after an upgrade | The new binary migrated those databases, and the old one cannot read them | Fix the failing tenant (its journal), then run the upgrade again; or restore that tenant's pre-migration backup |
| `an import is running on: <names>` | An import (the admin's or a person's upload) is still reading its palace | Wait until `alice-server cli <name> import status` no longer says running, then run the command again |
| `another alice-server command, or an import, is running` | The script's lock is held | Wait until that command or the admin's `import` ends, then run this again |
| `the adoption failed … stays stopped` | The adopted database could not be swapped back | Run the `alice restore` command it names with `alice-server cli`, then `systemctl start alice@<name>` |
| `alice-server: a secret is never passed on the command line` | You put a key after `set-secret` | Run `config set-secret jev_api_key` alone and paste the key at the prompt |
| `unknown secret name; use one of: jev_api_key, embedding_api_key` | The secret's name is wrong | The Jev key is `jev_api_key` |
| `these tenants did not take it (is their daemon running?)` after `jev-key set` | A tenant's daemon was down | Start it (`systemctl start alice@<name>`), then run `jev-key set` again |
