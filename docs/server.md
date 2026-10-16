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

The script takes a lock, so two runs cannot overlap, and it is safe to run twice: a repeated command leaves the same state and says so. `import` is the exception: it can run for hours, and two may run at once. While one runs, `upgrade`, `add --team`, and `remove` or `adopt` of that tenant refuse to start, because they would restart the daemon the import needs.

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

The `claude mcp add` line keeps `${ALICE_TOKEN}` literal, in single quotes, so the token never lands in a saved configuration. The person sets `ALICE_TOKEN` in their shell.

The tenant file holds no secret. It sets `ALICE_HOME`, `ALICE_SERVER_PORT`, `ALICE_LOG=info`, `ALICE_BACKUP_DAILY=2`, `ALICE_BACKUP_WEEKLY=0` and `TZ`.

`TZ` is the person's time zone, `Asia/Bangkok` unless `--tz` names another IANA zone (it must exist under `/usr/share/zoneinfo`). The daemon runs in it, so an import reads MemPalace's local times with the offset of the person's own machine, as it would on their Mac; stored times stay UTC. Run `alice-server add <name> --tz <zone>` on an existing tenant to change its zone (it restarts that daemon). `upgrade` gives `TZ=Asia/Bangkok` to a tenant made before this line existed and keeps a zone set with `--tz`.

#### remove

`alice-server remove <name>` asks for confirmation (skip it with `--yes`), stops the daemon, removes the Caddy route, archives the home to `/srv/alice/backups/<name>-<UTC time>.tar.gz` (root only, mode 0600) and then deletes the user, the home and the tenant file. The archive stays until you delete it.

#### rotate

`alice-server rotate <name>` replaces the token and prints the new one once, as `add` does. The daemon refuses the old token at once. Hand the new token to the person; their agent configuration keeps working as soon as `ALICE_TOKEN` holds the new value.

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

`jev-key set` reads the key from the terminal with echo off, or from standard input; never from the command line or the environment. It keeps it in `/etc/alice/jev.key` (root's only, 0600) and gives it to every tenant, the team store included: into the tenant's own `secrets.toml` (0600, its user's), through its running daemon, with `judge.provider = jev`. It takes effect at once; nothing restarts. `alice-server add` gives every new tenant the key. Run `jev-key set` again to rotate the key everywhere. `jev-key status` says whether a key is set and, for each tenant, whether it holds the server's key, another one or none. It never prints a key or part of one.

**The key stays the owner's.** Each tenant file pins `ALICE_JUDGE_JEV_ENDPOINT` (Jev's own endpoint) and `ALICE_JUDGE_DAILY_BUDGET_USD`, and the environment wins over a person's settings: a person cannot send the shared key anywhere else or raise the budget, and their settings show the key as set, without any part of it. `upgrade` adds both lines to tenants made before them.

**Budget.** Each tenant's judge budget is 1.00 USD a day by default. The deployment-wide ceiling is the number of tenants times that: with four people and the team store, 5.00 USD a day. To change one tenant's, edit `ALICE_JUDGE_DAILY_BUDGET_USD` in `/etc/alice/tenants/<name>.env`, then `systemctl restart alice@<name>`; `upgrade` keeps it. When a tenant reaches its budget, precise recall answers with the search order and says so.

SP2's distillation (`distill.provider = jev`) will be switched on by the same command once it lands on this branch.

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
| Backups per tenant | 2 daily, no weekly (`ALICE_BACKUP_DAILY=2`, `ALICE_BACKUP_WEEKLY=0`); a tenant cannot make manual backups |
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

Your admin gives you three things: the MCP URL `https://<host>/u/<name>/mcp`, your token, and a project name for each repository (a short slug such as `my-app`).

Keep the token in an environment variable named `ALICE_TOKEN`. Set it in your shell profile or a secrets manager, not in a file you commit. Do not put the token in a URL.

```bash
read -rs ALICE_TOKEN && export ALICE_TOKEN    # paste the token, press Enter
```

### Claude Code

For one repository, write a `.mcp.json` in its root. It holds no secret, so you can commit it. Claude Code expands `${VAR}` in `url` and `headers`:

```json
{
  "mcpServers": {
    "alice": {
      "type": "http",
      "url": "${ALICE_URL}?project=<slug>",
      "headers": {
        "Authorization": "Bearer ${ALICE_TOKEN}"
      }
    }
  }
}
```

Set `ALICE_URL` to `https://<host>/u/<name>/mcp` in your environment, next to `ALICE_TOKEN`.

You can add the server from the command line instead. Use `--scope project` so it is written to `.mcp.json`, and single quotes so the shell does not expand the token:

```bash
claude mcp add --transport http --scope project alice \
  'https://<host>/u/<name>/mcp?project=<slug>' \
  --header 'Authorization: Bearer ${ALICE_TOKEN}'
```

With double quotes, your shell would put the real token into the saved configuration.

### Codex

Add this to `~/.codex/config.toml`:

```toml
[mcp_servers.alice]
url = "https://<host>/u/<name>/mcp"
bearer_token_env_var = "ALICE_TOKEN"
```

`bearer_token_env_var` names the variable. Codex reads the token from it and sends it as `Authorization: Bearer`. Codex has no per-repository project setting here, so name the project in each request (see below) or append `?project=<slug>` to the `url`.

### Other agents

Any client that speaks MCP over streamable HTTP works. Give it the URL and the header `Authorization: Bearer <token>`, with the token read from the environment.

### Which project a memory goes to

Alice picks the project in this order:

1. the `project` argument of the tool call;
2. the `?project=<slug>` on the MCP URL (at most 100 characters);
3. the `X-Alice-Project` header (the same limits; UTF-8, or percent-encoded UTF-8);
4. the folder name of the first workspace root the agent offers;
5. `global`.

Claude Code sends no workspace roots over HTTP, so without one of the first three every session lands in `global`. Its `headersHelper` can send the header from the folder it runs in. In `~/.claude/settings.json` (or the server's entry of `.mcp.json`):

```json
"headersHelper": "/path/to/alice-headers.sh"
```

with `alice-headers.sh` printing the headers as JSON, the token from the environment and the project from the git checkout's folder name, percent-encoded so a Thai folder name comes through (Node cannot send it as it is):

```sh
#!/bin/sh
name=$(basename "$(git rev-parse --show-toplevel 2>/dev/null || pwd)")
project=$(printf '%s' "$name" | python3 -c 'import sys, urllib.parse; print(urllib.parse.quote(sys.stdin.read()))')
printf '{"Authorization": "Bearer %s", "X-Alice-Project": "%s"}\n' "$ALICE_TOKEN" "$project"
```

A header that is not UTF-8 after decoding fails the call, rather than putting the memory in another project.

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

### Importing your MemPalace

Give your palace folder to your admin. The admin runs `alice-server import` (see above), checks the report with you and deletes the server's copy. Secrets are redacted on import.

## Troubleshooting

| What you see | Meaning | What to do |
|---|---|---|
| `alice-server: this CPU lacks AVX` | The VM's CPU type has no AVX | Set the CPU type to `host`, shut the VM down, start it again |
| `the new binary failed its self-test (killed by SIGILL: …)` | This binary's ONNX Runtime needs instructions the CPU lacks; nothing was installed | Build it with `deploy/server/build-release` on the server; a plain `cargo build` needs AVX2 |
| `the new binary failed its self-test (exit …; last output: …)` | It could not load or run the model (often no network for the first model download) | Read the last output; check that the server reaches huggingface.co, then run `upgrade` again |
| `write ALICE_PUBLIC_HOST=<the server's public IP or name> to /etc/alice/server.env first` | `/etc/alice/server.env` does not exist | Create it with `ALICE_PUBLIC_HOST=<host>` |
| `ALICE_PUBLIC_HOST is empty in /etc/alice/server.env` | The file exists but the host is empty | Write `ALICE_PUBLIC_HOST=<host>` to it |
| HTTP 401 from the MCP URL | The token is wrong or was rotated | Check `ALICE_TOKEN`; ask the admin for a new token |
| HTTP 404 from the MCP URL | The tenant name is wrong or was removed; `/u/team/` never exists | Check the URL with the admin |
| The agent cannot start the server entry | `ALICE_URL` or `ALICE_TOKEN` is not set in the agent's environment | Set them in the shell that starts the agent, then restart it |
| "memory daemon is starting (downloading the embedding model …)" | A new tenant is fetching its model | Wait a few minutes and try again |
| A shared write fails | No team store, or it did not answer | Ask the admin whether `alice-server add --team team` was run, and check `alice-server list` |
| "only the author or the admin can change a shared memory" | The memory was written by someone else | Ask the author, or the admin |
| "the team memory did not answer in time; this answer is personal only" | The team store was slow or down | Try again; tell the admin if it repeats |
| `alice-server list` warns about free space | Under 3 GB free | See [Disk budget](#disk-budget) |
| A tenant is not `active` in `list` | The daemon stopped | `journalctl -u alice@<name>`, then `systemctl restart alice@<name>` |
| `alice-server upgrade` exits with tenant names | Those tenants did not restart | The previous binary was restored; read `journalctl -u alice@<name>` |
| `… still down: <names> … cannot start on the old one` after an upgrade | The new binary migrated those databases, and the old one cannot read them | Fix the failing tenant (its journal), then run the upgrade again; or restore that tenant's pre-migration backup |
| `an import is running for: <names>` | An import is still embedding | Wait until it ends, then run the command again |
| `the adoption failed … stays stopped` | The adopted database could not be swapped back | Run the `alice restore` command it names with `alice-server cli`, then `systemctl start alice@<name>` |
| `alice-server: a secret is never passed on the command line` | You put a key after `set-secret` | Run `config set-secret jev_api_key` alone and paste the key at the prompt |
| `unknown secret name; use one of: jev_api_key, embedding_api_key` | The secret's name is wrong | The Jev key is `jev_api_key` |
| `these tenants did not take it (is their daemon running?)` after `jev-key set` | A tenant's daemon was down | Start it (`systemctl start alice@<name>`), then run `jev-key set` again |
