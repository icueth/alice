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

A tenant cannot reach the admin's network. The service unit allows only the public internet and localhost, and tenant mode refuses settings that point at private or loopback addresses. A tenant also cannot change the settings the admin owns: the Jev endpoint, model and daily budget (pinned in its tenant file, because the Jev key is the owner's), its data directory, its port, its backup location and rotation, its secrets backend (always the private file), whether their store keeps secrets as written (pinned in its tenant file: [secrets](#secrets)), and the allowlist of exceptions to redaction (the tenant file's alone).

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
alice-server setup-line <name>
alice-server list
alice-server upgrade <binary>
alice-server prune
alice-server import <name> <palace-dir> [--dry-run]
alice-server adopt <name> <backup-file> [--cloud-denied <slug,slug...>]
alice-server jev-key set
alice-server jev-key status
alice-server distill on|off <name>|--all
alice-server distill status
alice-server secrets keep|redact <name>|--all
alice-server secrets status
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
Hand <name> the URL and the token over a private channel, the token as a file named alice-token. On their machine
they open Claude Code or Codex and say:
  Read docs/setup-server.md from the Alice repository (https://github.com/icueth/alice) and follow every step.
  My address is https://<host>/u/<name> and my token file is ~/Downloads/alice-token.
That keeps the token in ~/.config/alice/token (0600), connects the MCP and sets up capture. To set up capture
alone, or to repair it (alice-server setup-line <name> prints it again):
  curl -fsSL https://<host>/u/<name>/api/v1/capture/setup.sh | sh -s -- https://<host>/u/<name>
```

The token is shown once. Hand it to the person over a private channel, as a file named `alice-token`: [setup-server.md](setup-server.md) has their agent keep it in `~/.config/alice/token` (0600), where the MCP helper, the Codex wrapper and capture read it, so it never sits in an environment variable or a saved configuration. If the tenant already exists, `add` prints its URL (not the token) and stops. If anything fails, it undoes what it created.

The tenant file holds no secret. It sets `ALICE_HOME`, `ALICE_SERVER_PORT`, `ALICE_LOG=info`, `ALICE_BACKUP_DAILY=2`, `ALICE_BACKUP_WEEKLY=0` and `TZ`, the Jev pins below, `ALICE_PRIVACY_REDACT` (whether the person's store keeps secrets as written: [secrets](#secrets)), and `ALICE_CAPTURE_RAW_RETENTION=180d`: captured transcripts (SP2 capture) age out after 180 days, while imported history and memories stay. The environment wins over the person's settings, so they cannot keep transcripts longer. To change one tenant's, edit that line, then `systemctl restart alice@<name>`; `upgrade` adds the line to tenants made before it and keeps one the admin changed.

`TZ` is the person's time zone, `Asia/Bangkok` unless `--tz` names another IANA zone (it must exist under `/usr/share/zoneinfo`). The daemon runs in it, so an import reads MemPalace's local times with the offset of the person's own machine, as it would on their Mac; stored times stay UTC. Run `alice-server add <name> --tz <zone>` on an existing tenant to change its zone (it restarts that daemon). `upgrade` gives `TZ=Asia/Bangkok` to a tenant made before this line existed and keeps a zone set with `--tz`.

#### remove

`alice-server remove <name>` asks for confirmation (skip it with `--yes`), stops the daemon, removes the Caddy route, archives the home to `/srv/alice/backups/<name>-<UTC time>.tar.gz` (root only, mode 0600) and then deletes the user, the home and the tenant file. The archive stays until you delete it.

#### rotate

`alice-server rotate <name>` replaces the token and prints the new one once, as `add` does, with the setup line. The daemon refuses the old token at once. Hand the new token to the person; on their machine the setup line finds the stored token refused, asks for the new one and keeps it in `~/.config/alice/token`, where their MCP setup and capture read it.

#### setup-line

`alice-server setup-line <name>` prints the person's capture setup line again (below, "The setup line"). It holds no token. The team store has no address, so it has no setup line.

#### list

```
name             port   state      version    size     url
<name>           47801  active     0.1.0      1234 MB  https://<host>/u/<name>
team             47802  active     0.1.0      56 MB    (team, no URL)
free: 19.0 GB on /; remove archives: 0 MB
```

`list` shows one row per tenant, then the free space on the disk that holds `/srv/alice` and the size of the `remove` archives. Below 3 GB free it prints a warning. It never prints a token.

#### upgrade

First install this release's `deploy/server/alice-server` as `/usr/local/sbin/alice-server`, as in the one-time setup: an older script misses what the new release needs (the retention pin, `setup-line`, `distill`).

`alice-server upgrade <binary>` checks that the new binary runs, then runs its self-test: `alice model check` in a throwaway home under `/tmp` loads the embedding model and embeds one text. The model is kept in `/var/cache/alice-server/models` (about 136 MB, downloaded the first time). If the self-test dies (an "Illegal instruction" is named as such), fails or takes longer than 5 minutes, nothing is installed. Otherwise it keeps the current binary as `/usr/local/bin/alice.prev` and the current ONNX Runtime as `/usr/local/lib/alice/libonnxruntime.so.1.prev`, installs the new ones (the library when the binary comes from `build-release`, from `../lib/alice` beside it), and self-tests the installed binary. Then it restarts every tenant and waits for each to answer. If the installed binary fails its self-test, or any tenant does not come back, it restores the previous binary and library, restarts everything again and exits with an error (naming the tenants that failed). It prints the free space before the restarts, because a schema migration copies each tenant's database once. When every tenant is back it runs [prune](#prune), and it installs the daily prune timer on a server set up before it existed. On a server with no binary yet it is the first install.

#### prune

A schema migration first copies the tenant's database to `alice-pre-migration-v<N>-<UTC time>.db` in its `backups` folder, the way back if the migration goes wrong. The daemon alone keeps each copy for 30 days and the newest for ever, so every upgrade with a migration left one more full copy per tenant, enough to fill the disk. `alice-server prune` keeps a copy only until a daily backup newer than it exists for that tenant (the tenant has run on the migrated database since, and that backup is a newer restore point), and at most the newest copy per tenant in any case. It prints each copy it removes, its size and why, then the total:

```
removed ann's alice-pre-migration-v8-20261015T090000Z.db (1.2 GB: a newer daily backup exists)
pruned 1 pre-migration copy, 1.2 GB freed
```

It runs by itself at the end of every `upgrade` (only when every tenant came back: a failed upgrade keeps the copies) and once a day from the systemd timer `alice-server-prune.timer`, which `add` and `upgrade` install and which runs `/usr/local/sbin/alice-server prune`. Each tenant's own user removes its files. It never touches daily, manual or before-restore backups. While another `alice-server` command runs (an upgrade, `add`, `remove` and the like) it skips that day and says so; imports do not stop it. Check the timer with `systemctl list-timers alice-server-prune.timer` and its last run with `journalctl -u alice-server-prune`.

#### import

A person who has no `alice` binary can still bring their MemPalace palace. They hand the palace folder to you (for example, you receive it with `scp`). Then:

```
alice-server import <name> <palace-dir> --dry-run
alice-server import <name> <palace-dir>
```

The script moves the folder into the tenant's `import-inbox/` (owned by the tenant, mode 0700), runs the importer as the tenant against its own daemon, and prints the report. It deletes the copy when the job ends, whether it worked or not.

- A **real import consumes `<palace-dir>`**: the folder is moved, not copied. Keep your own copy if you need one.
- A **dry run works on a copy** and keeps your folder. It reports what would be imported and changes nothing.
- The importer follows the tenant's [secrets](#secrets) switch: a tenant that redacts has secrets removed (or, when its privacy mode blocks them, the drawers held back, and the report lists them); a tenant that keeps secrets stores them as written. Wing, room and project names are always redacted.
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
- It redacts every memory, raw chunk and history record again with the tenant's rules, and drops the judge cache. A memory whose redacted text equals another one's is moved to the trash. A tenant that keeps secrets ([secrets](#secrets)) keeps the adopted ones as written: set the switch before you adopt.
- Settings, secrets, the token and the Jev key stay the tenant's own: only the database is replaced. So do the person's cloud rules: the projects they kept from the cloud on their machine (their `privacy.cloud_denied_projects`, and repositories whose `.alice.toml` says `cloud_denied = true`) are not denied on the tenant unless you name them with `--cloud-denied`, which sets the tenant's list once it is up. The report's `projects_with_roots` lists the projects that had a working tree on the machine, the ones to ask about.
- The tenant starts again and rebuilds its vector index from the stored embeddings (no re-embedding, unless the source used another model). If it does not come back, its own database is put back.

It prints a report: memories, chunks, facts and projects, the paths cleared and the redactions. Facts, entity names and topics are taken as they are, not redacted again. The file is rewritten at the end (`VACUUM`), so nothing the redaction replaced or the person had deleted stays in it. Delete the backup file afterwards: it holds what the person's machine stored, before the server's redaction.

#### jev-key

On this server Jev is on for everyone, with the owner's one Jev key. People never see or supply a key.

```
sudo alice-server jev-key set        # paste the key at the prompt (it is not shown), or pipe it in
sudo alice-server jev-key status
```

`jev-key set` reads the key from the terminal with echo off, or from standard input; never from the command line or the environment. It keeps it in `/etc/alice/jev.key` (root's only, 0600) and gives it to every tenant, the team store included: into the tenant's own `secrets.toml` (0600, its user's), through its running daemon. It takes effect at once; nothing restarts. **It changes only the key**: a tenant's `judge.provider` and its distillation are its own settings, and neither `jev-key set` nor a rotation touches them. `alice-server add` gives every new tenant the key and `judge.provider = jev`. Run `jev-key set` again to rotate the key everywhere. `jev-key status` says whether a key is set and, for each tenant, whether it holds the server's key, another one or none. It never prints a key or part of one.

**The key stays the owner's.** Each tenant file pins `ALICE_JUDGE_JEV_ENDPOINT` (Jev's own endpoint) and `ALICE_JUDGE_DAILY_BUDGET_USD`, and the environment wins over a person's settings: a person cannot send the shared key anywhere else or raise the budget, and their settings show the key as set, without any part of it. `upgrade` adds both lines to tenants made before them.

**Budget.** Each tenant's judge budget is 1.00 USD a day by default. The deployment-wide ceiling is the number of tenants times that: with four people and the team store, 5.00 USD a day. To change one tenant's, edit `ALICE_JUDGE_DAILY_BUDGET_USD` in `/etc/alice/tenants/<name>.env`, then `systemctl restart alice@<name>`; `upgrade` keeps it. When a tenant reaches its budget, precise recall answers with the search order and says so.


#### distill

Distillation turns a tenant's captured sessions into a few short memories, with Jev and the shared key, inside the tenant's judge budget (SP2; see [Capture for team members](#capture-for-team-members)). It is a per-tenant choice:

```
sudo alice-server distill on <name>       # or --all
sudo alice-server distill off <name>      # or --all
sudo alice-server distill status
```

`on` and `off` set `distill.provider` (`jev` or `off`) through each tenant's running daemon, so it applies at once; a tenant whose daemon does not answer is named and the command fails (run it again). `on` says which tenants' judge is not `jev`: their distillation waits until it is. `status` lists every tenant with its distillation (`on`, `off`, or `unknown` when its daemon does not answer) and its judge, and the default for new tenants. It never shows a key.

**New tenants** follow the deployment default in `/etc/alice/distill.default`: one line, `on` or `off`; without the file, `off`. `add` refuses a file that holds anything else, before it creates anything. Changing the file changes no existing tenant, and neither `jev-key set` nor `upgrade` ever changes a tenant's choice. To make new tenants start with distillation on:

```
echo on | sudo tee /etc/alice/distill.default
```

#### secrets

Whether a person's own store keeps the credentials they share (passwords, tokens, connection strings) as written, so their agents can recall them later, or redacts them before storing. The owner's decision (2026-10-17): every person's store may keep them; the shared team store always redacts, because everyone reads it; and **anything sent to a cloud service is always redacted first**, whatever the store keeps: the Jev judge (precise recall included), distillation and a remote embedder never receive a secret.

```
sudo alice-server secrets keep <name>       # or --all (the team store is skipped)
sudo alice-server secrets redact <name>     # or --all
sudo alice-server secrets status
```

`keep` and `redact` pin `ALICE_PRIVACY_REDACT=false` or `=true` in the tenant file and restart that daemon (the pin is read at start). A tenant whose pin already says so is left alone, and the command refuses while an import runs on a daemon it would restart. The environment wins over the person's settings, and a tenant's `config.toml` can never turn redaction off, so only the admin changes it: a person's `config set privacy.redact false` is refused. `keep` is refused for the team store, and `keep --all` leaves it redacting. `status` shows the default for new tenants and each tenant's switch; a tenant file made before the pin existed shows `redact`.

- **What changes.** Only new writes: remembered, updated, captured and imported text. Redaction is destructive: what was redacted before stays `[REDACTED:…]` and cannot be restored. After `redact`, what was kept stays in the store until the person deletes or edits it.
- **Shared memories.** A `shared: true` memory is redacted before it leaves the person's daemon, whatever their switch, and the team store redacts whatever reaches it.
- **The agent's instructions follow the switch.** A tenant that keeps secrets tells its agents (in the MCP instructions) that credentials may be stored with `remember` and must never go into `shared: true` memories; one that redacts tells them never to store secrets. The MCP instructions are the live source: the Alice block the setup guide wrote into the person's `~/.claude/CLAUDE.md` and `~/.codex/AGENTS.md` is written once and does not change with the switch, so after `keep` or `redact` tell the person to run Step 5 of [setup-server.md](setup-server.md) again (it reads the switch and rewrites the block's last line).
- **No cut splits a secret.** Capture trims a tool's output and cuts long turns into chunks. In every mode it finds the secrets in the text first and never cuts inside one: a secret a trim would split is kept whole on the kept side (left out whole when that would keep more than 8,192 more characters), a chunk cut moves to before it, and one longer than a chunk stays whole in a chunk over the limit (a secret too large for any chunk, over 32 KB, is left out whole). Every request to Jev and every remote embedder input redacts each stored chunk on its own, so a secret kept whole in a chunk is one its detector finds there. A field stored on one line (a command, a subagent's prompt, a slash command's arguments) is searched with its line breaks, and a secret written over several lines keeps them there: the body of a private key pasted without its `BEGIN` line is known only by its lines. A title made from a memory's text is cut from that text redacted.
- **The allowlist is the admin's.** `privacy.allowlist` exempts values from redaction, so on a tenant it comes from the tenant file only (`ALICE_PRIVACY_ALLOWLIST='["^value$"]'`, then restart the tenant): a person's `config set privacy.allowlist …` and a `config.toml` that sets one are refused. Text that leaves for Jev or a remote embedder ignores the allowlist for the built-in detectors anyway; it exempts a value from storage redaction, never from egress. Custom patterns only add detectors, so a person may still add their own.

**New tenants** follow `/etc/alice/secrets.default`: one line, `keep` or `redact`; without the file, `redact`. `add` refuses a file that holds anything else, before it creates anything, and gives the team store `redact` whatever the file says. Changing the file changes no existing tenant. To let every new person keep their credentials:

```
echo keep | sudo tee /etc/alice/secrets.default
```

#### cli

`alice-server cli <name> <alice arguments...>` runs the `alice` command as the tenant's Unix user against that tenant's own daemon. Use it for a person's settings, or for a key of their own:

```
alice-server cli <name> config get judge
alice-server cli <name> config set judge.provider jev
alice-server cli <name> config set-secret jev_api_key
```

`config set-secret jev_api_key` reads the key from standard input. Run it as shown and paste the key when it asks, or pipe it in from a file or a secret manager. The script refuses a secret on the command line, because a command line is visible to other users in the process list and in shell history. Never type the key as an argument.

Settings are TOML values or plain text, and they take effect at once. The daemon refuses a few settings in tenant mode (for example `privacy.redact = false`, which is the admin's [secrets](#secrets) switch, or a private-network URL).

### Shared team memory (admin)

The shared team memory lives in one extra daemon named `team`:

```
alice-server add --team team
```

This creates the store, prints its token once, and links every existing personal tenant to it by copying the token into each tenant's home (mode 0600). A tenant added later is linked when it is created. The team store has **no URL of its own**: Caddy has no route to it, and only the personal daemons on the same machine reach it, over loopback. `alice-server rotate team` replaces its token in every personal home; no restart is needed. Manage the store with `alice-server cli team …`, which gives the admin full control over its memories. The team store has its own judge, cloud policy, backups and disk use.

### Privacy settings you control

- **The server redacts unless the admin chose keep.** Text arrives inside TLS and is redacted on the server, with the tenant's rules, before anything is stored or logged, unless the admin set the tenant to keep secrets as written ([secrets](#secrets)). Either way, nothing sent to Jev or another cloud service carries a secret, and the team store always redacts.
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
| Captured transcripts | about 16.8 KB per chunk with its two backups, as for a drawer: at the owner's measured pace (about 1,060 to 2,100 chunks a day, [benchmarks/sp2.md](benchmarks/sp2.md)) 3.2 to 6.4 GB at the 180-day retention |
| Schema migrations | one copy of each database per migration, 1 to 1.5 GB for a large tenant: [prune](#prune) keeps it only until a newer daily backup exists, and at most the newest per tenant, so a tenant holds one for about a day after an upgrade with a migration |

Rough totals: a large tenant uses about 4.4 GB with its two backups, before captured transcripts (up to about 6 GB more at 180 days, at the owner's pace). Three smaller tenants use about 1.1 GB each. If four large tenants share one 28 GB disk, they do not fit with a 3 GB reserve.

The guards act in this order:

1. `alice-server list` warns below 3 GB free.
2. Imports are refused when they would leave less than 2 GB free.
3. The backup job skips a copy it has no room for.
4. As the last line, the daemon goes read-only and spools writes.

**When free space is under 3 GB:**

- Delete old `remove` archives in `/srv/alice/backups` that you no longer need.
- Run `alice-server prune` to remove the pre-migration copies a newer daily backup has replaced.
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

- The server redacts secrets in everything you send, unless your admin set your tenant to keep them as written (`alice-server secrets keep <name>`; you cannot change it yourself). Then the credentials you share are stored and recalled as written, for your agents only: they never go into shared memories, and nothing sent to Jev or another cloud service carries them. Secrets redacted before the switch stay redacted.
- **Your repository's `.alice.toml` does not apply.** The server never sees your repository. To keep a project away from cloud services such as Jev, ask your admin to add it to your `privacy.cloud_denied_projects`.
- The admin can read your memory. Do not store what you would not show the admin.

### Shared team memory (person)

If your admin created the team store, you can save a memory for the whole team. Tell your agent:

> Save this to the shared memory.

In Thai: "บันทึกอันนี้ลงความจำส่วนกลางด้วย". The agent then writes the memory to the team store only; it does not keep a personal copy.

- **Every team member can read it.** Do not share anything private. Secrets in a shared memory are always redacted, even when your own store keeps them.
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
5. Only after step 4, connect your machine to your tenant with [setup-server.md](setup-server.md) (a tenant that has captured anything is no longer fresh, and `adopt` refuses it). It stops the local Alice taking writes: it removes the local capture hooks (`alice hooks uninstall claude`, and `codex`; otherwise every session is captured twice and starts with two briefings), turns local distillation off (`alice config set distill.provider off`), and points the `alice` MCP entry at your tenant. Two stores that both take writes drift apart, and nothing merges them back. Keep the local home as a read-only fallback.

The server clears your machine's folder paths from the projects and transcripts, and redacts everything again with its rules (unless your admin set your tenant to keep secrets: then what your Mac kept stays as written). Your Jev key is set on the tenant separately.

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

**What is redacted.** The server redacts every text again with its own rules, exactly as `alice import mempalace` does: API keys, tokens, passwords and private keys are removed or, if your privacy mode blocks them, the drawer is held back. If your admin set your tenant to keep secrets, they are imported as written instead (names of wings and rooms are still redacted). The report lists the held-back ones by kind of secret. The uploaded archive is deleted from the server as soon as it is unpacked, and the unpacked copy as soon as the import ends.

If you would rather not run anything, you can still give your palace folder to your admin, who runs `alice-server import` (see above).

## Capture for team members

A person's Claude Code and Codex sessions can be saved to their own tenant as they happen: every finished turn goes into the tenant's raw tier, in the session's project, where `recall` finds it with `include_raw`. Where distillation is on (the admin turns it on per tenant with `alice-server distill on <name>`, see [below](#distillation-and-the-shared-jev-key)), ended sessions are also distilled into a few short memories by Jev. The person installs no Alice binary: capture needs `sh`, `curl` and the agent. What is kept and what never is, and how distillation works, are in [capture.md](capture.md).

### The setup line

The admin gives each person one line, to run on their own machine:

```
curl -fsSL https://<host>/u/<name>/api/v1/capture/setup.sh | sh -s -- https://<host>/u/<name>
```

`alice-server add <name>` and `alice-server rotate <name>` print it; `alice-server setup-line <name>` prints it again.

- **Run it after the MCP setup.** [setup-server.md](setup-server.md) (which an agent can run for the person) writes `~/.config/alice/token` and `~/.config/alice/url` first; the setup line then asks nothing. Without those files it asks for the token once, on the terminal with typing hidden, and writes both files (0600, in a 0700 `~/.config/alice`). It never takes the token from the command line or the environment, and never keeps a second copy.
- **Nothing is written before the token is checked.** The script checks the address (`https://`, or `http://` to `127.0.0.1` or `localhost` for tests) and asks `GET /api/v1/capture/status` whether the token works. A refused token from the file is reported, and the script asks once for a new one. Pointed at a local daemon instead of a tenant, it stops and says so.
- **Updates.** The tenant hook script updates itself from your Alice server once a day, over https, and updates the plugin's `hooks.json` when that changes. Re-run the setup line only to repair an install, or once if `status` shows no `hook script:` line (a script from before self-updates). So after an `alice-server upgrade` that changes the capture scripts, nobody has to do anything: each machine picks the new script up within a day, at its next agent event. A re-run of the line updates everything in place, keeps the person's `briefing=off` choice, and reuses `~/.config/alice/token` and `~/.config/alice/url` without asking.
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

Beside Codex's `config.toml`, when setup or uninstall changed it: `config.toml.bak-alice` (0600), the file as it was just before.

**Claude Code.** Setup runs `claude plugin marketplace add ~/.config/alice/claude-marketplace` (or `marketplace update alice-local` on a re-run) and `claude plugin install alice-capture@alice-local --scope user`. Claude Code then merges the plugin's hooks itself. When the `claude` command is missing or fails, setup prints the two commands to type inside Claude Code instead: `/plugin marketplace add "<that folder>"` and `/plugin install alice-capture@alice-local`. The plugin runs this on `SessionStart`, `Stop`, `SubagentStop`, `PreCompact` and `SessionEnd`, with a 10-second timeout:

```
sh "$HOME/.config/alice/alice-capture.sh" hook claude-code || true
```

`|| true` keeps a missing script (after an uninstall, say) from ever failing an agent's turn.

**Codex.** When `codex` is installed, setup writes `$CODEX_HOME/hooks.json` (default `~/.codex/hooks.json`) with the same five events and `hook codex` (`SessionEnd` gets 3 seconds, Codex's limit). It writes the file only when it does not exist, or is still exactly the one setup wrote before. When the person has a `hooks.json` of their own, it changes nothing and prints the entries to add.

When `config.toml` already has hooks of its own (`[[hooks.<Event>]]` tables, such as OpenRig's), Codex reads one format per layer, so setup puts the same five hooks there instead: `[[hooks.<Event>]]` tables in a marked block (`# >>> alice capture (Codex) >>>` … `# <<< alice capture (Codex) <<<`) at the end of the file, after a copy of the old file to `config.toml.bak-alice`. No other line changes. The edit is checked with `python3`'s TOML reader (Python 3.11 or later, or `tomli`): the new file is written only if it reads back as what the old one meant plus Alice's five hooks. A block that is already Alice's is left byte for byte (so Codex keeps trusting it), and tables Codex puts inside it (its `[hooks.state."…"]` trust records) are kept. Without such a `python3`, or when the file cannot take the tables (an inline `hooks.<Event> = [...]` list), setup changes nothing and prints the block to add. Updates of the hook script do not touch the block; a re-run of setup repairs it.

Either way, **Codex runs a new hook only after the person trusts it**, and setup never does that for them: the next time they open Codex it asks "Hooks need review", and they choose **Trust all and continue** (or review the Alice entries with `/hooks`). Setup's summary ends with that line.

### How it works

- **The hook returns at once.** It reads the agent's JSON on standard input, starts a background copy of itself with the session's details (no token among them), and exits 0. Only `SessionStart` waits, at most 2 seconds, for the briefing.
- **The background copy sends the new bytes.** It waits 2 seconds after a stop-type event (the transcript file can lag), takes a lock for that transcript, and posts the bytes after the last offset to `POST /api/v1/capture/transcript`. Bodies are at most 2 MiB, growing to 8 MiB when a turn is larger. The server keeps the cursor, stores only complete turns, and answers with the next offset; it never reads a path on the person's machine.
- **Nothing is lost when the server is away.** On a network error or a server error the offset stays, and the next event of that session tries again: the transcript file itself is the queue. Each `SessionStart` also finishes up to five other sessions whose transcripts grew meanwhile, or that were left in the middle of a turn: a transcript whose last turn the server could not close yet (a tool call without its result) and that has not changed for 6 hours (a crash, a kill, a closed app: no `SessionEnd` will come) is sent as the session end, so its last turn is stored and the session ends. That close is sent once per hold; again only when the server did not answer, or answered that the tenant's capture rules wait. A session's events (and every session start, for every captured session) also send up to five subagent transcripts beside the session's that the script has no state for: a subagent killed in its first run never sends `SubagentStop`.
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

- The text arrives inside TLS, unredacted, and the tenant redacts it with its own rules before anything is stored or logged, unless the admin set it to keep secrets ([secrets](#secrets)). Either way, distillation sends Jev only redacted text, and a distilled memory never holds a secret.
- **A repository's `.alice.toml` is not mirrored.** The script reads two lines of it, at the git top level (else the session's folder), on the person's machine: `no_capture = true` (nothing of that session is sent, and the script's offset moves past those turns, so they are never sent later either) and `cloud_denied = true` (sent with `no_distill=1`: stored, never distilled). Only plain `key = true` lines are seen, not other TOML forms. A `.alice.toml` the script cannot read skips the event and keeps the offset (fail closed).
- **The tenant's settings decide the rest:** `capture.excluded_projects` (never captured) and `privacy.cloud_denied_projects` (never sent to Jev), set with `alice-server cli <name> config set …`.
- **Custom patterns and block mode from a repository's `.alice.toml` do not apply here.** They wait for a client that reads the repository on the person's machine (the deferred `alice remote`). The tenant's own `privacy.custom_patterns` apply.

### Which project a captured session lands in

The script names the project after the folder of the git repository the session runs in (not `$HOME`; outside git it names none, and the session goes to `global`). It sends that name as the `X-Alice-Project` header, percent-encoded so any folder name travels, and the tenant resolves it as it resolves the header over MCP: an existing slug or alias, else a new project of that name. The `alice-mcp-headers` helper and the Codex wrapper of [setup-server.md](setup-server.md) send the same folder name, so a repository's memories and its captured sessions land together.

**An MCP `?project=` pin does not reach capture.** If a repository's `.mcp.json` pins `?project=web` while its folder is `acme-web`, memories go to `web` and transcripts to `acme-web`. Pin the folder's name instead, or make one project a name of the other: `alice-server cli <name> projects merge acme-web web --yes` moves everything of `acme-web` into `web` and makes `acme-web` an alias, so later captures land in `web` too.

### Retention

Captured transcripts are kept **180 days** on every tenant, pinned by the line `ALICE_CAPTURE_RAW_RETENTION=180d` in `/etc/alice/tenants/<name>.env`. The environment wins over the tenant's `config.toml`, so a person's own settings cannot keep them longer; the admin changes the line (`90d` for the heaviest tenant, say) and runs `systemctl restart alice@<name>`. The daily job `capture_retention` deletes only captured chunks: imported MemPalace history and every memory stay, distilled ones included. `alice-server add` writes the line, and `upgrade` adds it to tenants made before it (keeping one the admin changed) and prints `captured transcripts kept 180d for: <names>`. To keep a tenant's transcripts for good, set the line to `ALICE_CAPTURE_RAW_RETENTION=forever`; do not delete it, because `upgrade` adds `180d` back to a tenant without the line.

As an estimate, carrying over the 16.8 KB per imported drawer (with its two backups) that the [Disk budget](#disk-budget) measures to captured chunks, the owner's measured pace (about 1,060 to 2,100 chunks a day, [benchmarks/sp2.md](benchmarks/sp2.md)) holds 3.2 to 6.4 GB of captured transcripts at 180 days, and a lighter user (300 a day) about 0.9 GB.

### Distillation and the shared Jev key

On this server Jev is on for everyone with the owner's one key (see [jev-key](#jev-key)): once `alice-server jev-key set` has provisioned it, `add` sets `judge.provider = jev` for each new tenant, and `jev-key set` (a rotation too) changes only the key. Distillation (`distill.provider = jev`) is a per-tenant choice the admin makes with `alice-server distill on|off <name>|--all`; `alice-server distill status` shows each tenant's setting. New tenants follow `/etc/alice/distill.default` (one line, `on` or `off`; `off` when the file is missing), see [distill](#distill). A tenant added before the key was set stays on the offline judge, and its distillation waits: set its judge with `alice-server cli <name> config set judge.provider jev`. The open-source default is off, and without a server key distillation cannot run.

- A person never sees or supplies the key. The setup line's summary says whether distillation is on (`distillation: on`), from the tenant's own setting.
- **Spend is per tenant**, inside its judge budget (`ALICE_JUDGE_DAILY_BUDGET_USD`, 1.00 USD a day by default), of which distillation may use half (`distill.budget_share = 0.5`). **The deployment-wide ceiling** is the number of daemons times that budget: 5.00 USD a day with four people and the team store, of which distillation can be at most 4 × 0.50 = 2.00 USD (the team store captures nothing). Expected spend is far lower: 0.0012 to 0.0224 USD per distilled session, measured on three real sessions ([benchmarks/sp2.md](benchmarks/sp2.md)).
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

`uninstall` removes the plugin and the marketplace (or prints `/plugin uninstall alice-capture@alice-local` and `/plugin marketplace remove alice-local` to type in Claude Code), removes the Codex `hooks.json` only if it is still exactly the one setup wrote, takes Alice's block out of `config.toml` (exactly that block: other tables inside it, such as Codex's trust records, stay; the old file is kept as `config.toml.bak-alice`; a block that is not exactly Alice's needs `python3` 3.11 or later, or it is left and the person told to remove it), and then asks before deleting the script, `state/`, `capture.conf`, `capture.log`, `paused-until` and `update-checked`. It never deletes `token` or `url`, which the MCP setup uses. Restart open Claude Code and Codex sessions afterwards, so none still runs the removed hooks.

### What the admin's script provides

Capture needs these from `alice-server` (SP2 spec §9.5):

| Interface | What `alice-server` does |
|---|---|
| Print the setup line | `alice-server add <name>` and `rotate <name>` print the setup line above; `alice-server setup-line <name>` prints it again |
| Pin capture retention | `add` writes `ALICE_CAPTURE_RAW_RETENTION=180d` into the tenant's file; `upgrade` adds it where it is missing and keeps a value the admin changed |
| Jev with the shared key | Once `alice-server jev-key set` has provisioned the key, `add` sets `judge.provider = jev` for the tenant, and `distill.provider` from `/etc/alice/distill.default` (`off` without it); `alice-server distill on\|off <name>\|--all` changes a tenant's distillation and `distill status` shows it; `jev-key set` changes only the key |
| Adoption | `alice-server adopt` moves a database whole, so its capture cursors and turn keys travel with it; it clears the captured sessions' transcript paths and file identities, which a tenant never reads |

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
| Codex sessions are not captured | The hooks are not trusted yet, or the person has their own `hooks.json` (or a `config.toml` setup could not add to) | Start Codex and choose **Trust all and continue** at "Hooks need review" (or trust them in `/hooks`); add the entries setup printed |
| No briefing at session start | `briefing=off`, `capture.briefing = false`, a resumed session, or a server slower than 2 s | Check `status`; the agent still calls `context` itself |
| Memories and transcripts of one repository in two projects | An MCP `?project=` pin differs from the folder name | See [Which project a captured session lands in](#which-project-a-captured-session-lands-in) |
| `capture.log`: "capture hook update refused …" or "capture hook update check failed …" | The day's update check got no usable script; the installed one stays | Nothing: it tries again a day later. If it repeats, run the setup line again |
| `capture.log`: "the plugin's hooks.json changed, and claude plugin update failed …" | An update changed the plugin's hooks, and the `claude` command did not reload them | Run the setup line again |
| `capture.log`: "updated … hooks.json with the hook script; Codex runs the changed hooks only after you trust them again …" | An update changed the Codex hooks | Open Codex and choose **Trust all and continue** at "Hooks need review" (or trust the Alice entries in `/hooks`) |

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
