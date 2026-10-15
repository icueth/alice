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

A tenant cannot reach the admin's network. The service unit allows only the public internet and localhost, and tenant mode refuses settings that point at private or loopback addresses. A tenant also cannot change the settings the admin owns: its data directory, its port, its backup location and rotation, its secrets backend (always the private file), and the redaction switch (always on).

## For the admin

### Requirements

- A Linux server with systemd, root access, and `caddy` installed.
- An **x86-64 CPU with AVX2.** The embedding runtime stops with "Illegal instruction" without it. `alice-server` checks for AVX and AVX2 before `add` and before `upgrade`, and refuses to go on if they are missing.
- On Proxmox, set the VM's CPU type to `host` (or `x86-64-v3`). Then shut the VM down and start it again. A reboot from inside the guest does not apply a CPU type change. Proxmox's default "QEMU Virtual CPU" has no AVX.
- Memory: each tenant daemon is capped at 3 GB (`MemoryMax=3G`); the measured use is about 800 MB. Disk: see [Disk budget](#disk-budget).

### One-time setup

1. Build or copy the `alice` binary to `/usr/local/bin/alice`.
2. Install the script `deploy/server/alice-server` as `/usr/local/sbin/alice-server`.
3. Write the public host to `/etc/alice/server.env`:

   ```
   ALICE_PUBLIC_HOST=<host>
   ```

   The script reads the host from this file, so no address is written into the script. It stops with a message if the file or the value is missing.

4. Run `alice-server add <name>` for the first person. The first run sets up the base: directories, the systemd unit `alice@.service` and the Caddy configuration.

`alice-server` manages `/etc/caddy/Caddyfile` from one template. The first time it runs, it copies your existing Caddyfile to `/etc/caddy/Caddyfile.pre-alice` and never overwrites that copy. Compare the two files before the switch. After every change it runs `caddy validate`, then reloads Caddy. If validation fails, it undoes the change and exits with an error. The template also moves Caddy's admin API from `localhost:2019` to a Unix socket, so a tenant cannot reach it.

The script takes a lock, so two runs cannot overlap, and it is safe to run twice: a repeated command leaves the same state and says so.

### Commands

```
alice-server add <name>
alice-server add --team team
alice-server remove <name> [--yes]
alice-server rotate <name>
alice-server list
alice-server upgrade <binary>
alice-server import <name> <palace-dir> [--dry-run]
alice-server cli <name> <alice arguments...>
```

A tenant name matches `^[a-z][a-z0-9-]{0,15}$` (a lowercase letter, then up to 15 lowercase letters, digits or hyphens).

#### add

`alice-server add <name>` creates the user, the home directory (`/srv/alice/<name>`, mode 0700), a tenant file in `/etc/alice/tenants/` and a port (the lowest free one in 47801-47899). It starts the daemon, waits for it to answer (the first start downloads the model, about 118 MB), adds the Caddy route and prints the details once:

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

The tenant file holds no secret. It sets `ALICE_HOME`, `ALICE_SERVER_PORT`, `ALICE_LOG=info`, `ALICE_BACKUP_DAILY=2` and `ALICE_BACKUP_WEEKLY=0`.

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

`alice-server upgrade <binary>` checks that the new binary runs, keeps the current one as `/usr/local/bin/alice.prev`, installs the new one, restarts every tenant and waits for each to answer. If any tenant does not come back, it restores the previous binary, restarts everything again and exits with an error that names the tenants that failed. It prints the free space first, because a schema migration copies each tenant's database once.

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

#### cli

`alice-server cli <name> <alice arguments...>` runs the `alice` command as the tenant's Unix user against that tenant's own daemon. Use it to set a person's settings or Jev key:

```
alice-server cli <name> config get judge
alice-server cli <name> config set judge.provider jev
alice-server cli <name> config set-secret typesafe
```

`config set-secret typesafe` reads the key from standard input. Run it as shown and paste the key when it asks, or pipe it in from a file or a secret manager. The script refuses a secret on the command line, because a command line is visible to other users in the process list and in shell history. Never type the key as an argument.

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
| A tenant's home | up to about 2 GB for a large palace (fully embedded), plus the 118 MB model |
| Scheduled backups | one copy of the database each; about 1-1.5 GB for a large tenant |
| Backups per tenant | 2 daily, no weekly (`ALICE_BACKUP_DAILY=2`, `ALICE_BACKUP_WEEKLY=0`); a tenant cannot make manual backups |
| `remove` archives | a gzip of the removed home, in `/srv/alice/backups` |
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
3. the folder name of the first workspace root the agent offers;
4. `global`.

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

You do not need a client binary for this. Ask your admin to run, on the server:

```
alice-server cli <name> config set-secret typesafe     # the admin pastes your key at the prompt
alice-server cli <name> config set judge.provider jev
```

Send the key over a private channel, never in a chat room or an issue. Without a key, the judge stays on the offline rules.

### Importing your MemPalace

Give your palace folder to your admin. The admin runs `alice-server import` (see above), checks the report with you and deletes the server's copy. Secrets are redacted on import.

## Troubleshooting

| What you see | Meaning | What to do |
|---|---|---|
| `alice-server: this CPU lacks AVX/AVX2` | The VM's CPU type has no AVX2 | Set the CPU type to `host` or `x86-64-v3`, shut the VM down, start it again |
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
| `alice-server: a secret is never passed on the command line` | You put a key after `set-secret` | Run `config set-secret typesafe` alone and paste the key at the prompt |
