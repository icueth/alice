# Alice Server Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let up to five people (first four: `icue`, `joe-hermes`, `joe`, `petch`) use Alice from one server. Each person has their own daemon and token behind Caddy, points their own `alice` client at it with `alice remote set <url>`, and imports their own MemPalace palace into it, all inside a fixed disk budget.

**Architecture:**
- **Server:** the daemon gains a tenant mode (`alice daemon run --tenant`). It never runs git on a path a client sent. It takes the project identity the client sends, and it keeps a mirror file of each project's `.alice.toml` that it stores as that project's root, so every existing fail-closed privacy path applies unchanged. Its settings refuse LAN, loopback and out-of-home targets.
- **Client:** `DaemonClient` gains a remote target. It verifies the ping proof before sending the token, detects the project and redacts text locally, and queues writes, which it replays itself.
- **Import:** the client builds a redacted import bundle from the person's palace and uploads it. M3's import job reads it on the server through a third `Backend` (`Backend::Bundle`).
- **Admin:** a bash script (`alice-server`) manages users, systemd units and Caddy routes.

**Tech Stack:** Rust 2024 (workspace `crates/alice-core`, `crates/alice-daemon`, `crates/alice`), axum 0.8, reqwest 0.13 (rustls, ring), tokio, rusqlite; bash and shellcheck for the admin script; systemd and Caddy 2.11 on Ubuntu 24.04.

**Spec:** `docs/superpowers/specs/2026-10-15-alice-server-mode-design.md` (read it; this plan argues from it).

## Global Constraints

- The repository is public: no committed file (code, tests, docs, scripts, this plan) names the server's address. Write `<server-ip>` or `https://<host>/u/<name>`; tests use documentation addresses such as `203.0.113.7`. The real address lives only in `/etc/alice/server.env` on the server and in git-ignored notes.
- Work only in `/Users/icue/Code/AI/project-alice-srv` (branch `feat/server-mode`). Never write in `/Users/icue/Code/AI/project-alice` or `/Users/icue/Code/AI/project-alice-m3`.
- **Every cargo command** uses this prefix (one heavy run at a time on this Mac; throttled; no secrets in the environment):
  `cd /Users/icue/Code/AI/project-alice-srv && source "$HOME/.cargo/env" && env -u TYPESAFE_API_KEY -u ALICE_EMBEDDING_API_KEY CARGO_TARGET_DIR=/Users/icue/Code/AI/project-alice-srv/target nice -n 15 cargo <subcommand> -j 4 …`
  The steps below write only the `cargo …` part. Always add the prefix.
- Never read, source, print or grep any `.env` file. Check a file for the Jev key only with `grep -c -F -- "$TYPESAFE_API_KEY" <file>`.
- **Secret-shaped test strings** are built with `concat!`, for example `concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789")`.
- **Committing:**
  - stage files by name, never `git add -A` or `git add .`;
  - every commit message ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`;
  - push only `feat/server-mode`; never push `main`, merge, tag, release or publish.
- `rm -rf` only with guarded variables (`"${VAR:?}"`).
- Never touch the real MemPalace palace, `~/.claude*`, `~/.codex`, or this Mac's services (no launchctl, systemctl or schtasks here).
- **Style:**
  - match the surrounding code: doc comments in full English sentences, user-facing errors that say what happened and what to do;
  - the token is never in an error, log or `Debug` output;
  - a client string is never echoed in an error beyond its field name.
- **Values fixed by the spec:**

  | Item | Value |
  |---|---|
  | Tenant name | `^[a-z][a-z0-9-]{0,15}$` |
  | Ports | 47801–47899 |
  | `.alice.toml` text sent by a client | at most 64 KiB |
  | `key` | at most 1024 bytes |
  | `name` | at most 200 characters |
  | `reason` | at most 500 characters |
  | Mirror directory | first 32 hex characters of SHA-256 of the key, under `<ALICE_HOME>/client-projects/` |
  | Ping capabilities | `client-project`, plus `tenant` under `--tenant` |
  | Remote timeouts | ping (connect included) 3 s; connect on later calls 5 s; recall, remember, get, update, facts, undo and reviews 15 s; context 8 s; ingest 60 s; the `mcp.admin_tools` flag 3 s; backup and maintenance 3 h |
  | Replay | `alice mcp` every 30 s; one-shot commands within a 2 s budget; lock file `spool/.replay.lock` |
  | Environment variables | `ALICE_REMOTE_URL`, `ALICE_REMOTE_TOKEN` (both or neither) |
  | Config file | `<ALICE_HOME>/remote.json`, mode 0600 |
  | Unit | `MemoryMax=3G`, `IPAddressAllow=localhost`, `IPAddressDeny=10.0.0.0/8 172.16.0.0/12 192.168.0.0/16 169.254.0.0/16 100.64.0.0/10 fc00::/7 fe80::/10` |
  | Tenant file | `ALICE_HOME`, `ALICE_SERVER_PORT`, `ALICE_LOG=info`, `ALICE_BACKUP_DAILY=2`, `ALICE_BACKUP_WEEKLY=0` |
  | Bundle upload | at most 4 GiB; refused when it would leave less than 2 GB free; one at a time; unused uploads swept after 1 hour; file `import-tmp/<ULID>/bundle.sqlite3`; format `alice-import-bundle`, version 1 |
  | `alice-server list` | warns below 3 GB free |
  | Base branches | `origin/feat/m2-judge-settings` and `origin/feat/m3-import` are merged; merge both again in Task 19. Keep changes to M3's importer additive (a backend and an entry point) |

## Review Focus

These input classes and failure modes are implied by the spec but not exercised by any task's main tests. Each one has a test added to the task that owns the code.

1. **A remote URL with a trailing slash, a path, upper-case scheme or a port** (`HTTPS://host:8443/u/ann/`) must normalize to one base, and requests must not produce `//api`. Test in Task 8.
2. **A `.alice.toml` that is a directory, a symlink to elsewhere, or non-UTF-8** on the client must send `unreadable` (fail closed), not crash or send `none`. Test in Task 10.
3. **A client that switches projects mid-session** in `alice mcp` (roots change, or cwd differs per call) must send each call's own `client_project`. The 60 s cache is keyed by cwd, never global. Test in Task 10.
4. **A spool entry written by an older local client** (with `cwd`, no `client_project`, no `idempotency_key`) must replay to the server under the right project, exactly once. Test in Task 11.
5. **The server answering HTML or an empty body** (Caddy's 404 for a wrong tenant name, a 502 page) must give the "did not prove it holds this token" or "unreachable" message, never a JSON decode error. Test in Task 9.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/alice-core/src/api.rs` (modify) | `ClientProject`, `ClientRules`, their validation; `client_project` on four requests |
| `crates/alice-core/src/settings.rs` (modify) | `parse_project` public with the unreadable marker; `validate_tenant`; `host_is_private`; the two variables in `NON_SETTING_ENV` |
| `crates/alice-daemon/src/paths.rs` (modify) | `client_projects_dir()`, `remote_file()` |
| `crates/alice-daemon/src/config.rs` (modify) | `SettingsHandle` tenant mode: validation on load, poll and patch; a missing root fails closed |
| `crates/alice-daemon/src/client_projects.rs` (create) | The server-side mirror of a client's `.alice.toml` |
| `crates/alice-daemon/src/service.rs` (modify) | `ServiceOptions::tenant`, `Scope`, `Scoped`, `choose(Scope)`, `rules_fail_closed(Scope)`, `is_tenant()`, the `SpoolSink` impl |
| `crates/alice-daemon/src/http.rs` (modify) | Ping capabilities; `ErrorBody.refused` and `retry_later` |
| `crates/alice-daemon/src/daemon.rs` (modify) | `DaemonOptions::tenant` |
| `crates/alice-daemon/src/spool.rs` (modify) | `SpoolSink`, `Applied`, and a generic `drain` |
| `crates/alice-daemon/src/remote_config.rs` (create) | `remote.json` and the environment: load, save, remove, URL rules |
| `crates/alice-daemon/src/client.rs` (modify) | The remote target: connect, proof, capabilities, timeouts, errors, request preparation hooks |
| `crates/alice-daemon/src/remote_prepare.rs` (create) | Client-side project detection, rules reading and redaction |
| `crates/alice-daemon/src/remote_replay.rs` (create) | Client replay of the spool to the server, under a lock |
| `crates/alice-daemon/src/mcp.rs` (modify) | Background replay; the admin flag through the cached client |
| `crates/alice-daemon/src/compat.rs` (modify) | `CompatCall.client_project`, passed to the requests it builds |
| `crates/alice-core/src/import/bundle.rs` (create) | The import bundle: `BundleWriter`, `BundleReader` (`PalaceReader`), `check`, `snapshot_of` |
| `crates/alice-core/src/import/palace.rs`, `import/mod.rs` (modify) | `Backend::Bundle` and its arms; `ImportRequest::bundle` |
| `crates/alice-daemon/src/import_bundle.rs` (create) | Client side: snapshot a palace and write a redacted bundle |
| `crates/alice-daemon/src/importer.rs` (modify, additive) | Bundle as the job's snapshot; bundle preflight |
| `crates/alice-daemon/tests/support/caddy.rs` (create) | Test harness: two tenant daemons behind a Caddy emulator |
| `crates/alice-daemon/tests/server_mode.rs` (create) | End-to-end remote tests through the emulator |
| `crates/alice/src/main.rs`, `crates/alice/src/cmd.rs` (modify) | `alice remote …`, `daemon run --tenant`, refusals, remote status and doctor, replay after commands |
| `crates/alice/tests/cli.rs` (modify) | CLI tests against a real `--tenant` daemon on loopback |
| `deploy/server/alice-server` (create) | The admin script |
| `deploy/server/tests/run.sh` (create) | Stub-based tests of the admin script |
| `docs/server.md` (create), `README.th.md`, `README.md` (modify) | Documentation |

---

### Task 1: Core types — `ClientProject`, the unreadable marker, the environment variables

**Files:**
- Modify: `crates/alice-core/src/api.rs` (add the types next to `RememberRequest`; add a field to `RecallRequest` near line 86, `RememberRequest` near line 160, `ContextRequest` near line 195 and `IngestRequest` near line 375)
- Modify: `crates/alice-core/src/settings.rs` (`NON_SETTING_ENV` at lines 35–51; `parse_project` near line 1925)
- Modify: every struct literal of those four requests that lists all fields (`cargo check` finds them; at least `crates/alice/src/cmd.rs` for `RecallRequest` and `ContextRequest`)
- Test: in-file `#[cfg(test)] mod tests` of `api.rs` and `settings.rs`

**Interfaces:**
- Produces:
  - `alice_core::api::ClientProject { key: String, slug: String, name: String, remote: Option<String>, rules: ClientRules }`;
  - `alice_core::api::ClientRules::{None, File { text: String }, Unreadable { reason: String }}`;
  - `alice_core::api::CLIENT_RULES_MAX_BYTES: usize = 65_536`;
  - `ClientProject::validate(&self) -> alice_core::error::Result<()>`;
  - field `client_project: Option<ClientProject>` on `RecallRequest`, `RememberRequest`, `ContextRequest` and `IngestRequest`;
  - `alice_core::settings::parse_project(name: &str, text: &str) -> Result<ProjectSettings, SettingError>` (now `pub`);
  - `alice_core::settings::UNREADABLE_MARKER: &str`;
  - `alice_core::settings::unreadable_marker_text(reason: &str) -> String`.

- [ ] **Step 1: Write the failing tests.** In `crates/alice-core/src/api.rs` tests (create `#[cfg(test)] mod tests { use super::*; … }` if there is none):

```rust
    fn cp(key: &str, slug: &str) -> ClientProject {
        ClientProject {
            key: key.into(),
            slug: slug.into(),
            name: slug.into(),
            remote: None,
            rules: ClientRules::None,
        }
    }

    #[test]
    fn client_project_round_trips_in_the_documented_shape() {
        let p = ClientProject {
            key: "github.com/icueth/alice".into(),
            slug: "alice".into(),
            name: "alice".into(),
            remote: Some("github.com/icueth/alice".into()),
            rules: ClientRules::File { text: "[privacy]\ncloud_denied = true\n".into() },
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["rules"]["state"], "file");
        assert_eq!(v["rules"]["text"], "[privacy]\ncloud_denied = true\n");
        let back: ClientProject = serde_json::from_value(v).unwrap();
        assert_eq!(back, p);
        let none: ClientRules = serde_json::from_str(r#"{"state":"none"}"#).unwrap();
        assert_eq!(none, ClientRules::None);
        let bad: ClientRules =
            serde_json::from_str(r#"{"state":"unreadable","reason":"permission denied"}"#).unwrap();
        assert_eq!(bad, ClientRules::Unreadable { reason: "permission denied".into() });
    }

    #[test]
    fn a_request_without_client_project_serialises_as_before() {
        let r = RememberRequest { content: "x".into(), ..Default::default() };
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("client_project").is_none());
        let r: RecallRequest = serde_json::from_str(r#"{"query":"q"}"#).unwrap();
        assert!(r.client_project.is_none());
    }

    #[test]
    fn client_project_validation_refuses_what_the_spec_refuses() {
        assert!(cp("github.com/a/b", "b").validate().is_ok());
        let long = "k".repeat(1025);
        for (case, p) in [
            ("empty key", cp("", "b")),
            ("global key", cp("global", "b")),
            ("long key", cp(&long, "b")),
            ("control char in key", cp("a\u{7}b", "b")),
            ("empty slug", cp("k", "")),
            ("slug not slugified", cp("k", "My Repo")),
        ] {
            let e = p.validate().unwrap_err().to_string();
            assert!(!e.contains("My Repo") && !e.contains(&long), "{case}: echoed: {e}");
        }
        let mut p = cp("k", "b");
        p.name = "n".repeat(201);
        assert!(p.validate().is_err(), "long name");
        let mut p = cp("k", "b");
        p.remote = Some("r".repeat(1025));
        assert!(p.validate().is_err(), "long remote");
        let mut p = cp("k", "b");
        p.rules = ClientRules::File { text: "#".repeat(CLIENT_RULES_MAX_BYTES + 1) };
        assert!(p.validate().is_err(), "large rules");
        let mut p = cp("k", "b");
        p.rules = ClientRules::Unreadable { reason: "r".repeat(501) };
        assert!(p.validate().is_err(), "long reason");
    }
```

In `crates/alice-core/src/settings.rs` tests:

```rust
    #[test]
    fn the_unreadable_marker_fails_with_a_clear_message() {
        let text = unreadable_marker_text("permission denied\nsecond line");
        assert!(text.starts_with(UNREADABLE_MARKER));
        assert_eq!(text.lines().count(), 1, "one line: {text:?}");
        let e = parse_project("/srv/x/.alice.toml", &text).unwrap_err();
        assert!(e.message.contains("the client could not read this project's .alice.toml"), "{}", e.message);
        assert!(e.message.contains("permission denied"), "{}", e.message);
        // Ordinary text still parses.
        assert!(parse_project("f", "[privacy]\ncloud_denied = true\n").unwrap().privacy.cloud_denied);
    }

    #[test]
    fn the_remote_variables_are_not_settings() {
        for v in ["ALICE_REMOTE_URL", "ALICE_REMOTE_TOKEN"] {
            assert!(NON_SETTING_ENV.contains(&v), "{v}");
        }
    }
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p alice-core client_project` and `cargo test -p alice-core unreadable_marker`
Expected: compile errors (`ClientProject`, `UNREADABLE_MARKER` not found).

- [ ] **Step 3: Implement.** In `api.rs`, near `RememberRequest`:

```rust
/// Largest `.alice.toml` a remote client may send (spec §4.1).
pub const CLIENT_RULES_MAX_BYTES: usize = 64 * 1024;

/// The project a remote client detected for its working directory, with that working tree's
/// privacy rules as the client read them (server mode, spec §3.3 and §4.1). A server uses it
/// instead of running git on a path it does not have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClientProject {
    /// `ProjectRef::key` as detected on the client (a normalized remote, or `path:<dir>`).
    pub key: String,
    pub slug: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    pub rules: ClientRules,
}

/// The working tree's `.alice.toml`, as the client found it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ClientRules {
    /// The working tree has no `.alice.toml`.
    None,
    /// Its content, at most [`CLIENT_RULES_MAX_BYTES`].
    File { text: String },
    /// It exists but could not be read; the rules fail closed.
    Unreadable { reason: String },
}

impl ClientProject {
    /// The limits of spec §4.1. A refusal names the field, never the value.
    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str| Err(AliceError::invalid(format!(
            "client_project.{what}; update the alice client, or report it if it is current"
        )));
        if self.key.is_empty() || self.key.len() > 1024 {
            return bad("key must be 1 to 1024 bytes");
        }
        if self.key.chars().any(char::is_control) {
            return bad("key must not contain control characters");
        }
        if self.key == "global" {
            return bad("key must not be \"global\" (send no client_project for global)");
        }
        if self.slug.is_empty() || crate::normalize::slugify(&self.slug) != self.slug {
            return bad("slug must be a non-empty slug (lower case, digits and -)");
        }
        if self.name.chars().count() > 200 {
            return bad("name must be at most 200 characters");
        }
        if self.remote.as_ref().is_some_and(|r| r.len() > 1024) {
            return bad("remote must be at most 1024 bytes");
        }
        match &self.rules {
            ClientRules::File { text } if text.len() > CLIENT_RULES_MAX_BYTES => {
                bad("rules.text must be at most 64 KiB")
            }
            ClientRules::Unreadable { reason } if reason.chars().count() > 500 => {
                bad("rules.reason must be at most 500 characters")
            }
            _ => Ok(()),
        }
    }
}
```

Use whatever `Result` and `AliceError` imports `api.rs` already has (add `use crate::error::{AliceError, Result};` if they are missing). Add this field to `RecallRequest`, `RememberRequest`, `ContextRequest` and `IngestRequest`, right after each one's `cwd` field:

```rust
    /// Server mode: the project the remote client detected (spec §4.1). Wins over `cwd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub client_project: Option<ClientProject>,
```

In `settings.rs`:
- add `"ALICE_REMOTE_URL", "ALICE_REMOTE_TOKEN",` to `NON_SETTING_ENV`;
- make `parse_project` `pub`;
- add the marker and its handling:

```rust
/// The first line of a server-side mirror whose client could not read the project's
/// `.alice.toml` (server mode, spec §2.2 item 3). [`parse_project`] refuses it, so the rules fail closed.
pub const UNREADABLE_MARKER: &str = "# alice: the client could not read .alice.toml";

/// The one-line mirror text for an unreadable project file; `reason` is flattened to one line.
pub fn unreadable_marker_text(reason: &str) -> String {
    let reason: String = reason
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(500)
        .collect();
    format!("{UNREADABLE_MARKER} ({})\n", reason.trim())
}
```

At the top of `parse_project`, before the TOML parse:

```rust
    if let Some(rest) = text.strip_prefix(UNREADABLE_MARKER) {
        let why = rest.lines().next().unwrap_or("").trim();
        return Err(err(
            name,
            format!("the client could not read this project's .alice.toml {why}; its rules fail closed until the client can read it"),
        ));
    }
```

- [ ] **Step 4: Fix the struct literals.** Run `cargo check --workspace --all-targets`. Add `client_project: None,` to every literal the compiler reports. Do not add `..Default::default()` where the code lists every field on purpose.

- [ ] **Step 5: Run the tests to see them pass**

Run: `cargo test -p alice-core client_project`, then `cargo test -p alice-core unreadable_marker`, then `cargo test -p alice-core remote_variables`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/alice-core/src/api.rs crates/alice-core/src/settings.rs crates/alice/src/cmd.rs
git commit -m "feat(core): client_project on scoped requests and the unreadable-rules marker (server mode)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

Add any other file Step 4 touched to the `git add` line by name.

---

### Task 2: Tenant settings rules — `validate_tenant` and `host_is_private`

**Files:**
- Modify: `crates/alice-core/src/settings.rs` (new functions after `impl Settings { fn validate … }`)
- Test: in-file tests

**Interfaces:**
- Produces:
  - `pub fn host_is_private(host: &str) -> bool`;
  - `pub fn validate_tenant(s: &Settings, home: &Path, port_from_file: bool) -> Vec<SettingError>`.

The table in spec §2.2 item 6 is binding. Rules:
- `backup.dir`, when set, must resolve inside `home`. A relative path is joined to `home`; `..` is resolved lexically; then the longest existing ancestor is canonicalized, so a symlink cannot escape. Compare against the canonical `home`.
- `embedding.endpoint` and `judge.jev.endpoint` must be `https://`, and their host must not satisfy `host_is_private`. `embedding.endpoint` is checked only when it is `Some`.
- `embedding.provider = Ollama` is refused.
- `sync.remote`, when `Some` and non-empty, is refused.
- `server.port` other than 0 is refused: the unit's `ALICE_SERVER_PORT` decides. The environment layer sets it in tenant mode, so `validate_tenant` sees the resolved value. Refuse only a value that comes from the file (see Task 3, which passes the resolved settings together with their sources). Implement it as a parameter: `validate_tenant(s, home, port_from_file: bool)`. Final signature: `pub fn validate_tenant(s: &Settings, home: &Path, port_from_file: bool) -> Vec<SettingError>`.
- `secrets.backend = Keychain` is refused.

`host_is_private` is true for:
- `localhost`, and any name ending in `.localhost`;
- IPv4 loopback, private, link-local (169.254/16), CGNAT (100.64/10), unspecified and broadcast;
- IPv6 loopback, unspecified, unique-local (fc00::/7) and link-local (fe80::/10);
- an IPv4-mapped IPv6 address whose IPv4 address is private.

Parse with `std::net::IpAddr`; strip `[ ]` first.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn private_hosts_are_recognised() {
        for h in ["localhost", "LOCALHOST", "x.localhost", "127.0.0.1", "127.1.2.3", "10.0.0.5",
                  "172.16.0.1", "172.31.255.1", "192.168.1.99", "169.254.1.1", "100.64.0.1",
                  "0.0.0.0", "::1", "[::1]", "fc00::1", "fd12::1", "fe80::1", "::ffff:192.168.1.1"] {
            assert!(host_is_private(h), "{h}");
        }
        for h in ["api.typesafe.ai", "8.8.8.8", "172.32.0.1", "100.128.0.1", "2001:4860::8888", "203.0.113.7"] {
            assert!(!host_is_private(h), "{h}");
        }
    }

    fn tenant_errors(toml: &str, home: &Path, port_from_file: bool) -> Vec<String> {
        let r = resolve(Some(toml), &[]).unwrap();
        validate_tenant(&r.settings, home, port_from_file).into_iter().map(|e| e.key).collect()
    }

    #[test]
    fn tenant_rules_refuse_each_row_of_the_table_and_allow_good_values() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let outside = d.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, home.join("escape")).unwrap();
        let refused = [
            (format!("[backup]\ndir = \"{}\"\n", outside.display()), "backup.dir"),
            ("[backup]\ndir = \"../outside\"\n".to_string(), "backup.dir"),
            ("[embedding]\nprovider = \"openai\"\nmodel = \"m\"\nendpoint = \"https://192.168.1.99/v1\"\n".into(), "embedding.endpoint"),
            ("[embedding]\nprovider = \"openai\"\nmodel = \"m\"\nendpoint = \"http://127.0.0.1:2019/v1\"\n".into(), "embedding.endpoint"),
            ("[embedding]\nprovider = \"openai\"\nmodel = \"m\"\nendpoint = \"http://api.openai.com/v1\"\n".into(), "embedding.endpoint"),
            ("[embedding]\nprovider = \"ollama\"\nmodel = \"m\"\n".into(), "embedding.provider"),
            ("[judge.jev]\nendpoint = \"http://localhost:9/v1\"\n".into(), "judge.jev.endpoint"),
            ("[judge.jev]\nendpoint = \"https://[fe80::1]/v1\"\n".into(), "judge.jev.endpoint"),
            ("[sync]\nremote = \"git@192.168.1.5:me/mem.git\"\n".into(), "sync.remote"),
            ("[secrets]\nbackend = \"keychain\"\n".into(), "secrets.backend"),
        ];
        for (toml, key) in &refused {
            assert!(tenant_errors(toml, &home, false).contains(&key.to_string()), "{key}: {toml}");
        }
        #[cfg(unix)]
        assert!(tenant_errors("[backup]\ndir = \"escape/b\"\n", &home, false).contains(&"backup.dir".to_string()), "symlink escape");
        assert!(tenant_errors("[server]\nport = 47801\n", &home, true).contains(&"server.port".to_string()));
        assert!(tenant_errors("[server]\nport = 47801\n", &home, false).is_empty(), "port from the environment");
        let allowed = [
            format!("[backup]\ndir = \"{}\"\n", home.join("backups").display()),
            "[backup]\ndir = \"backups/daily\"\n".to_string(),
            "[embedding]\nprovider = \"openai\"\nmodel = \"m\"\nendpoint = \"https://api.openai.com/v1\"\n".into(),
            "[judge]\nprovider = \"jev\"\n".into(),
            String::new(),
        ];
        for toml in &allowed {
            assert!(tenant_errors(toml, &home, false).is_empty(), "{toml}");
        }
    }
```

The embedding provider's settings may require keys the snippet does not set (`model`, an API key name). If `resolve` refuses a snippet for a reason other than the tenant rule, adjust the snippet minimally, keeping the URL under test.

- [ ] **Step 2: Run to see them fail.** Run: `cargo test -p alice-core tenant_rules` and `cargo test -p alice-core private_hosts`. Expected: compile errors.

- [ ] **Step 3: Implement**

```rust
/// Whether `host` names this machine, a private network or a link-local address: what a
/// server tenant's settings must not point at (spec R18). Names other than `localhost` are not
/// resolved here; the unit's `IPAddressDeny` covers a public name that resolves privately.
pub fn host_is_private(host: &str) -> bool {
    use std::net::{IpAddr, Ipv4Addr};
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    if h.eq_ignore_ascii_case("localhost") || h.to_ascii_lowercase().ends_with(".localhost") {
        return true;
    }
    let v4 = |a: Ipv4Addr| {
        let o = a.octets();
        a.is_loopback() || a.is_private() || a.is_link_local() || a.is_unspecified()
            || a.is_broadcast() || (o[0] == 100 && (64..128).contains(&o[1]))
    };
    match h.parse::<IpAddr>() {
        Ok(IpAddr::V4(a)) => v4(a),
        Ok(IpAddr::V6(a)) => {
            if let Some(m) = a.to_ipv4_mapped() {
                return v4(m);
            }
            let s = a.segments()[0];
            a.is_loopback() || a.is_unspecified() || (s & 0xfe00) == 0xfc00 || (s & 0xffc0) == 0xfe80
        }
        Err(_) => false,
    }
}

/// `p` (relative to `home` when relative) resolves inside `home`, symlinks included.
fn resolves_inside(home: &Path, p: &str) -> bool {
    use std::path::Component;
    let joined = home.join(p);
    let mut lexical = std::path::PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => { lexical.pop(); }
            Component::CurDir => {}
            other => lexical.push(other),
        }
    }
    // Canonicalize the longest existing ancestor, then add the rest back.
    let mut existing = lexical.clone();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (existing.file_name().map(|n| n.to_owned()), existing.parent()) {
            (Some(n), Some(parent)) => { rest.push(n); existing = parent.to_path_buf(); }
            _ => return false,
        }
    }
    let Ok(mut real) = existing.canonicalize() else { return false };
    for n in rest.into_iter().rev() {
        real.push(n);
    }
    let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    real.starts_with(&home)
}

fn check_tenant_endpoint(e: &mut Vec<SettingError>, key: &str, url: &str) {
    match url_parts(url) {
        Some(u) if u.scheme == "https" && !u.host.is_empty() && !host_is_private(u.host) => {}
        _ => e.push(err(key, "on a server tenant this must be an https:// URL of a public host (not localhost, a private network or a link-local address)")),
    }
}

/// The extra rules a server tenant's settings follow (spec §2.2 item 6, R18): no path outside
/// `home`, no URL into the machine or the LAN, nothing the server's admin owns.
/// `port_from_file`: `server.port` comes from `config.toml` rather than the environment.
pub fn validate_tenant(s: &Settings, home: &Path, port_from_file: bool) -> Vec<SettingError> {
    let mut e = Vec::new();
    if let Some(dir) = s.backup.dir.as_deref().filter(|d| !d.trim().is_empty())
        && !resolves_inside(home, dir)
    {
        e.push(err("backup.dir", "on a server tenant this must be a directory inside the tenant's data directory"));
    }
    if s.embedding.provider == EmbeddingProvider::Ollama {
        e.push(err("embedding.provider", "ollama is not available on a server tenant (it runs on this machine or the LAN); use local, openai or voyage"));
    }
    if let Some(url) = s.embedding.endpoint.as_deref() {
        check_tenant_endpoint(&mut e, "embedding.endpoint", url);
    }
    check_tenant_endpoint(&mut e, "judge.jev.endpoint", &s.judge.jev.endpoint);
    if s.sync.remote.as_deref().is_some_and(|r| !r.trim().is_empty()) {
        e.push(err("sync.remote", "sync is not available on a server tenant yet"));
    }
    if port_from_file && s.server.port != 0 {
        e.push(err("server.port", "the server's admin sets the port of a server tenant"));
    }
    if s.secrets.backend == SecretsBackend::Keychain {
        e.push(err("secrets.backend", "a server tenant keeps its keys in secrets.toml (\"file\"); there is no keychain on the server"));
    }
    e
}
```

- [ ] **Step 4: Run to see them pass.** Run: `cargo test -p alice-core tenant_rules` and `cargo test -p alice-core private_hosts`. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/alice-core/src/settings.rs
git commit -m "feat(core): tenant settings rules (no LAN, loopback or out-of-home targets; R18)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: `SettingsHandle` tenant mode

**Files:**
- Modify: `crates/alice-daemon/src/config.rs`:
  - the struct `SettingsHandle` near line 357;
  - `load` and `load_with_env` near lines 386–417;
  - `observe` near line 556;
  - `patch` near line 604;
  - `cloud_allowed_with`, `project_privacy`, `project_fail_closed` near lines 463–515.
- Test: in-file tests (helper `home()` exists).

**Interfaces:**
- Consumes: `alice_core::settings::{validate_tenant, Source}` (Task 2).
- Produces:
  - `SettingsHandle::load_with(home: &AliceHome, env: Vec<(String, String)>, tenant: bool) -> Arc<Self>`;
  - `SettingsHandle::load_tenant(home: &AliceHome) -> Arc<Self>` (the process environment, tenant on);
  - `SettingsHandle::is_tenant(&self) -> bool`.
  - `load` and `load_with_env` keep their meaning (tenant off).

**Behaviour:**
- A new field `tenant: Option<PathBuf>` holds the canonical home root.
- On load and on every poll, the resolved settings are checked with `validate_tenant(settings, home, port_from_file)`, where `port_from_file` is whether the source of `server.port` is the file (`resolved.sources.get("server.port")` is the file source, not `Source::Env`). On a violation the file is **not applied**: the settings become `settings::resolve(None, &env.vars)` (defaults plus environment), and the violations join the file errors. Locally an invalid file degrades; here a violation does too, so cloud consumers see `config_degraded()`.
- A patch whose result violates is refused with the violations, and the file is not written.
- In tenant mode, a project `root` whose directory does not exist fails closed: `ProjectSettings::fail_closed()`, with an error naming `<root>/.alice.toml`.

- [ ] **Step 1: Write the failing tests** (in `config.rs` tests):

```rust
    #[test]
    fn a_tenant_refuses_a_patch_that_points_at_the_lan() {
        let (_d, h) = home();
        let s = SettingsHandle::load_with(&h, Vec::new(), true);
        assert!(s.is_tenant());
        let e = s.patch(&one("judge.jev.endpoint", serde_json::json!("https://192.168.1.99/v1"))).unwrap_err();
        assert!(e.iter().any(|e| e.key == "judge.jev.endpoint"), "{e:?}");
        assert!(!h.config_file().exists() || !std::fs::read_to_string(h.config_file()).unwrap().contains("192.168"));
        // The same patch is fine for a local daemon.
        let (_d2, h2) = home();
        let local = SettingsHandle::load_with_env(&h2, Vec::new());
        assert!(!local.is_tenant());
        local.patch(&one("judge.jev.endpoint", serde_json::json!("https://192.168.1.99/v1"))).unwrap();
    }

    #[test]
    fn a_tenant_does_not_apply_a_hand_edited_file_that_breaks_its_rules() {
        let (_d, h) = home();
        std::fs::write(h.config_file(), "[sync]\nremote = \"git@10.0.0.5:m.git\"\n[search]\ndefault_max_tokens = 999\n").unwrap();
        let s = SettingsHandle::load_with(&h, Vec::new(), true);
        assert!(s.get().sync.remote.is_none());
        assert_ne!(s.get().search.default_max_tokens, 999, "nothing of a violating file applies");
        assert!(s.errors().iter().any(|e| e.key == "sync.remote"));
        assert!(s.config_degraded(), "cloud consumers fail closed");
    }

    #[test]
    fn a_tenant_port_from_the_environment_is_fine_and_from_the_file_is_refused() {
        let (_d, h) = home();
        let env = vec![("ALICE_SERVER_PORT".to_string(), "47801".to_string())];
        let s = SettingsHandle::load_with(&h, env.clone(), true);
        assert_eq!(s.get().server.port, 47801);
        assert!(s.errors().is_empty(), "{:?}", s.errors());
        let e = s.patch(&one("server.port", serde_json::json!(47802))).unwrap_err();
        assert!(e.iter().any(|e| e.key == "server.port"), "{e:?}");
    }

    #[test]
    fn a_tenant_fails_closed_for_a_project_root_that_is_not_there() {
        let (d, h) = home();
        let s = SettingsHandle::load_with(&h, Vec::new(), true);
        let gone = d.path().join("never-created");
        assert!(!s.cloud_allowed("proj", Some(&gone)));
        assert!(s.project_fail_closed(Some(&gone)));
        assert_eq!(s.project_privacy(Some(&gone)).redact_mode, RedactMode::Block);
        // A local daemon keeps today's meaning: no file, no project rules.
        let local = SettingsHandle::load_with_env(&h, Vec::new());
        assert!(local.cloud_allowed("proj", Some(&gone)));
    }
```

Use the existing test helper that builds a one-key patch map. If the file has none named `one`, add:

```rust
    fn one(k: &str, v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        let mut m = serde_json::Map::new();
        m.insert(k.into(), v);
        m
    }
```

- [ ] **Step 2: Run to see them fail.** Run: `cargo test -p alice-daemon --lib config::tests::a_tenant`. Expected: compile errors.

- [ ] **Step 3: Implement.**
  - Add `tenant: Option<PathBuf>` to `SettingsHandle`.
  - Move the body of `load_with_env` into `load_with(home, env, tenant)`; it sets `tenant: tenant.then(|| home.root().canonicalize().unwrap_or_else(|_| home.root().to_path_buf()))`. `load_with_env(home, env)` becomes `Self::load_with(home, env, false)`, and `load_tenant(home)` becomes `Self::load_with(home, process_env(), true)`.
  - Add a free function and use it instead of `resolve_layers` inside `load_with` and `observe`:

```rust
/// `resolve_layers`, then the tenant rules (spec §2.2 item 6): a file that breaks one is not
/// applied at all, and its violations are reported with the file's errors.
fn resolve_for(read: &Read, env: &EnvLayer, tenant: Option<&Path>) -> (Resolved, Vec<SettingError>) {
    let (resolved, mut errors) = resolve_layers(read, env);
    let Some(home) = tenant else { return (resolved, errors) };
    let port_from_file = !matches!(resolved.sources.get("server.port"), Some(Source::Env { .. }) | None);
    let violations = settings::validate_tenant(&resolved.settings, home, port_from_file);
    if violations.is_empty() {
        return (resolved, errors);
    }
    errors.extend(violations);
    let base = settings::resolve(None, &env.vars).unwrap_or_else(|_| default_resolved());
    (base, errors)
}
```

Check the actual shape of `resolved.sources` and `Source` in `alice_core::settings` (`resolve` records a source per key; a key left at its default may be absent or `Source::Default`). `port_from_file` must be true only when the file set the key. In `load_with`, when `resolve_for` returned violations, leave `ever_valid` false (an invalid file at start does that already, since the violations are in `file_errors`).

In `patch`, after `let r = settings::resolve(Some(&text), &self.env.vars)?;`:

```rust
        if let Some(home) = &self.tenant {
            let port_from_file = !matches!(r.sources.get("server.port"), Some(Source::Env { .. }) | None);
            let violations = settings::validate_tenant(&r.settings, home, port_from_file);
            if !violations.is_empty() {
                return Err(violations);
            }
        }
```

Add the project loader and use it in `cloud_allowed_with` (instead of `projects.get(root)`), `project_privacy` and `project_fail_closed`:

```rust
    /// The project file for `root`; on a server tenant a root that is not there fails closed
    /// (spec §2.2 item 4: rules this machine cannot know).
    fn project_load(&self, root: &Path, projects: &ProjectSettingsCache) -> ProjectLoad {
        if self.tenant.is_some() && !root.is_dir() {
            return ProjectLoad {
                settings: ProjectSettings::fail_closed(),
                error: Some(SettingError {
                    key: root.join(".alice.toml").display().to_string(),
                    message: "this project's rules are not on this server; they fail closed until its client sends them again".into(),
                }),
                loosening_pending: false,
            };
        }
        projects.get(root)
    }

    pub fn is_tenant(&self) -> bool {
        self.tenant.is_some()
    }
```

Import `ProjectLoad`, `ProjectSettings`, `SettingError` and `Source` from `alice_core::settings` as needed.

- [ ] **Step 4: Run the tests to see them pass**, then make sure the existing config tests still pass. Run: `cargo test -p alice-daemon --lib config::`. Expected: PASS (all).

- [ ] **Step 5: Commit**

```bash
git add crates/alice-daemon/src/config.rs
git commit -m "feat(daemon): settings tenant mode — tenant rules on load, poll and patch; a missing root fails closed

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: The mirror — `client_projects.rs`

**Files:**
- Create: `crates/alice-daemon/src/client_projects.rs`
- Modify: `crates/alice-daemon/src/lib.rs` (`pub mod client_projects;`), `crates/alice-daemon/src/paths.rs` (add `client_projects_dir()`)
- Test: in-file tests

**Interfaces:**
- Consumes: `ClientProject`, `ClientRules` and `unreadable_marker_text` (Task 1); `alice_core::project::ProjectRef`.
- Produces:
  - `AliceHome::client_projects_dir(&self) -> PathBuf` (`<root>/client-projects`);
  - `client_projects::mirror_dir(home: &AliceHome, key: &str) -> PathBuf`;
  - `client_projects::sync_mirror(home: &AliceHome, cp: &ClientProject) -> alice_core::error::Result<ProjectRef>`. It validates, writes or removes the mirror, and returns the `ProjectRef` whose `root` is the mirror directory. It is blocking: call it on the blocking pool.

- [ ] **Step 1: Write the failing tests** (in the new file):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alice_core::api::{ClientProject, ClientRules};

    fn home() -> (tempfile::TempDir, AliceHome) {
        let d = tempfile::tempdir().unwrap();
        let h = AliceHome::new(d.path().to_path_buf());
        h.ensure_dirs().unwrap();
        (d, h)
    }

    fn cp(rules: ClientRules) -> ClientProject {
        ClientProject { key: "github.com/me/app".into(), slug: "app".into(), name: "app".into(),
            remote: Some("github.com/me/app".into()), rules }
    }

    #[test]
    fn the_mirror_is_named_by_a_hash_and_holds_the_text() {
        let (_d, h) = home();
        let r = sync_mirror(&h, &cp(ClientRules::File { text: "[privacy]\ncloud_denied = true\n".into() })).unwrap();
        let dir = mirror_dir(&h, "github.com/me/app");
        assert_eq!(r.root.as_deref(), Some(dir.as_path()));
        assert_eq!((r.key.as_str(), r.slug.as_str(), r.name.as_str()), ("github.com/me/app", "app", "app"));
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name.len(), 32);
        assert!(name.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(dir.starts_with(h.client_projects_dir()));
        assert_eq!(std::fs::read_to_string(dir.join(".alice.toml")).unwrap(), "[privacy]\ncloud_denied = true\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
            assert_eq!(std::fs::metadata(dir.join(".alice.toml")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".tmp")).collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn none_removes_the_file_and_unreadable_writes_the_marker() {
        let (_d, h) = home();
        sync_mirror(&h, &cp(ClientRules::File { text: "[privacy]\nblock_secrets = true\n".into() })).unwrap();
        let file = mirror_dir(&h, "github.com/me/app").join(".alice.toml");
        sync_mirror(&h, &cp(ClientRules::None)).unwrap();
        assert!(!file.exists());
        assert!(file.parent().unwrap().is_dir(), "the directory stays: it is the project's root");
        sync_mirror(&h, &cp(ClientRules::Unreadable { reason: "permission denied".into() })).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with(alice_core::settings::UNREADABLE_MARKER));
        assert!(alice_core::settings::load_project(file.parent().unwrap()).is_err(), "fails closed");
    }

    #[test]
    fn an_invalid_client_project_writes_nothing() {
        let (_d, h) = home();
        let mut bad = cp(ClientRules::None);
        bad.slug = "Not A Slug".into();
        assert!(sync_mirror(&h, &bad).is_err());
        assert!(!h.client_projects_dir().exists() || std::fs::read_dir(h.client_projects_dir()).unwrap().next().is_none());
    }

    #[test]
    fn the_same_text_is_not_rewritten() {
        let (_d, h) = home();
        let rules = ClientRules::File { text: "[privacy]\ncloud_denied = true\n".into() };
        sync_mirror(&h, &cp(rules.clone())).unwrap();
        let file = mirror_dir(&h, "github.com/me/app").join(".alice.toml");
        let before = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        sync_mirror(&h, &cp(rules)).unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), before);
    }
}
```

- [ ] **Step 2: Run to see them fail.** Run: `cargo test -p alice-daemon --lib client_projects`. Expected: compile errors.

- [ ] **Step 3: Implement.** In `paths.rs`:

```rust
    /// Server mode: the copies of remote projects' `.alice.toml` (spec §2.2 item 3).
    pub fn client_projects_dir(&self) -> PathBuf {
        self.root.join("client-projects")
    }
```

Use the actual field name `AliceHome` uses for its root. `client_projects.rs`:

```rust
//! Server mode: the server's copy (mirror) of each remote project's `.alice.toml` (spec §2.2
//! item 3, ruling R1). The mirror directory is stored as the project's root, so every privacy path
//! that reads `<root>/.alice.toml` (the project rules cache, the redactors, the cloud policy, the
//! judge) applies a client's rules exactly as it applies a local working tree's.

use crate::paths::AliceHome;
use alice_core::api::{ClientProject, ClientRules};
use alice_core::error::{AliceError, Result};
use alice_core::project::ProjectRef;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

/// `<home>/client-projects/<first 32 hex characters of SHA-256(key)>`: never a client string.
pub fn mirror_dir(home: &AliceHome, key: &str) -> PathBuf {
    let digest = hex::encode(Sha256::digest(key.as_bytes()));
    home.client_projects_dir().join(&digest[..32])
}

fn io(e: std::io::Error) -> AliceError {
    AliceError::Other(format!(
        "cannot keep this project's privacy rules on the server ({e}); nothing was stored. Check the server's disk"
    ))
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

/// Written to a new private file, synced, renamed over `path`.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_file_name(format!(".alice.toml.{}.tmp", alice_core::ids::new_id()));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let done = (|| {
        let mut f = o.open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

/// Bring the mirror of `cp`'s project up to date and return the project with the mirror as
/// its root. Blocking (file system): call it on the blocking pool.
pub fn sync_mirror(home: &AliceHome, cp: &ClientProject) -> Result<ProjectRef> {
    cp.validate()?;
    let dir = mirror_dir(home, &cp.key);
    private_dir(&dir).map_err(io)?;
    let file = dir.join(".alice.toml");
    let wanted = match &cp.rules {
        ClientRules::None => None,
        ClientRules::File { text } => Some(text.clone()),
        ClientRules::Unreadable { reason } => Some(alice_core::settings::unreadable_marker_text(reason)),
    };
    match wanted {
        None => match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(e)),
        },
        Some(text) => {
            if std::fs::read_to_string(&file).ok().as_deref() != Some(text.as_str()) {
                write_private(&file, &text).map_err(io)?;
            }
        }
    }
    Ok(ProjectRef {
        key: cp.key.clone(),
        slug: cp.slug.clone(),
        name: cp.name.clone(),
        root: Some(dir),
        remote: cp.remote.clone(),
    })
}
```

- [ ] **Step 4: Run to see them pass.** Run: `cargo test -p alice-daemon --lib client_projects`. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/alice-daemon/src/client_projects.rs crates/alice-daemon/src/lib.rs crates/alice-daemon/src/paths.rs
git commit -m "feat(daemon): the server-side mirror of a client's .alice.toml (R1)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: Service tenant mode — `Scope`, `choose(Scope)`, `client_project` precedence

**Files:**
- Modify: `crates/alice-daemon/src/service.rs`:
  - `ServiceOptions` near line 178 and its `Default`;
  - `Service::start` near line 728, where the settings are loaded;
  - `choose` near line 1131;
  - every call site: `remember` near 1146, `rules_fail_closed` near 1192, `redact_remember` near 1257, `recall` near 1336, `context` near 1753, `ingest` near 2821.
- Modify: `crates/alice-daemon/src/spool.rs` line ~144–162 (the `scope` tuple and the `rules_fail_closed` call)
- Test: in-file tests of `service.rs` (helpers `svc()`, `quiet()`, `repo()`, `svc_with_grader()`, `KeywordGrader`, `recall_req`, `remember_deploy_notes`, `wait_settled`, `remember_id`, `patch_one` and `NOTE_DENIED` exist)

**Interfaces:**
- Consumes: `client_projects::sync_mirror` (Task 4); `SettingsHandle::load_with` and `load_tenant` (Task 3).
- Produces:
  - `pub struct Scope<'a> { pub project: Option<&'a str>, pub cwd: Option<&'a str>, pub client_project: Option<&'a ClientProject> }` (Copy, Default);
  - `pub trait Scoped { fn scope(&self) -> Scope<'_>; }` for `RememberRequest`, `RecallRequest`, `ContextRequest` and `IngestRequest`;
  - `ServiceOptions::tenant: bool` (default false);
  - `Service::is_tenant(&self) -> bool`;
  - `Service::rules_fail_closed(&self, scope: Scope<'_>) -> bool` (signature changed).

**Precedence** (spec §4.1):
1. If `client_project` is present, sync the mirror first, whatever wins.
2. An explicit `project` slug wins (`ProjectChoice::Slug`).
3. Else the mirrored `ProjectRef` (`ProjectChoice::Detected`).
4. Else `cwd`, only when not in tenant mode, through the existing resolver path (cache, then git on the blocking pool).
5. Else `ProjectChoice::Fallback`.

- [ ] **Step 1: Write the failing tests** (in `service.rs` tests):

```rust
    // ------------------------------------------------- server mode: client_project and tenant mode

    fn client_project(key: &str, slug: &str, rules: ClientRules) -> Option<ClientProject> {
        Some(ClientProject { key: key.into(), slug: slug.into(), name: slug.into(), remote: None, rules })
    }

    async fn tenant_svc() -> (tempfile::TempDir, Arc<Service>) {
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(
            AliceHome::new(d.path().to_path_buf()),
            &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
            ServiceOptions { tenant: true, ..quiet() },
        )
        .unwrap();
        (d, s)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_project_is_stored_with_its_mirror_as_the_root() {
        let (_d, s) = tenant_svc().await;
        assert!(s.is_tenant());
        let r = s.remember(RememberRequest {
            content: "Staging runs on Fly".into(),
            client_project: client_project("github.com/me/app", "app", ClientRules::File { text: "[privacy]\nblock_secrets = true\n".into() }),
            ..Default::default()
        }).await.unwrap();
        assert_eq!(r.project, "app");
        let root = s.read(|c| projects::root_of(c, "app")).await.unwrap().unwrap();
        assert_eq!(std::path::PathBuf::from(&root), crate::client_projects::mirror_dir(s.home(), "github.com/me/app"));
        // The mirror's block_secrets applies on the server.
        let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        let e = s.remember(RememberRequest {
            content: format!("deploy with {token}"),
            client_project: client_project("github.com/me/app", "app", ClientRules::File { text: "[privacy]\nblock_secrets = true\n".into() }),
            ..Default::default()
        }).await.unwrap_err();
        assert!(alice_core::redact::is_refusal(&e), "{e}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_tenant_ignores_cwd_and_an_explicit_slug_still_wins() {
        let (d, s) = tenant_svc().await;
        let cwd = repo(&d, "somewhere");
        let r = s.remember(RememberRequest { content: "a note".into(), cwd: cwd.clone(), ..Default::default() }).await.unwrap();
        assert_eq!(r.project, "global", "no git on a client path in tenant mode");
        s.remember(RememberRequest {
            content: "first app note".into(),
            client_project: client_project("k/app", "app", ClientRules::None),
            ..Default::default()
        }).await.unwrap();
        let r = s.remember(RememberRequest {
            content: "named note".into(),
            project: Some("global".into()),
            client_project: client_project("k/app", "app", ClientRules::None),
            ..Default::default()
        }).await.unwrap();
        assert_eq!(r.project, "global");
        // The local meaning of cwd is untouched.
        let (d2, local) = svc().await;
        let cwd2 = repo(&d2, "proj");
        let r = local.remember(RememberRequest { content: "local note".into(), cwd: cwd2, ..Default::default() }).await.unwrap();
        assert_eq!(r.project, "proj");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_mirrored_cloud_denial_keeps_the_judge_and_the_query_away() {
        let t = KeywordGrader::new("ZEBRA");
        let (d, s) = svc_with_grader(t.clone()).await;
        let denied = client_project("k/secret", "secret", ClientRules::File { text: "[privacy]\ncloud_denied = true\n".into() });
        // Never stored yet: the Task 22 path (query_cloud on the mirror root) denies the grading.
        let r = s.recall(RecallRequest { client_project: denied.clone(), ..recall_req("deploy notes", RecallMode::Precise, &None) }).await.unwrap();
        assert!(!r.reranked);
        assert_eq!(t.graded(), 0);
        // Stored: the judge's policy reads the mirror for later calls.
        for text in DEPLOY_NOTES {
            s.remember(RememberRequest { content: text.into(), kind: Some(Kind::Howto), client_project: denied.clone(), ..Default::default() }).await.unwrap();
        }
        wait_settled(&s).await;
        assert!(!s.cloud.cloud_allowed("secret"));
        let r = s.recall(RecallRequest { client_project: denied, ..recall_req("deploy notes", RecallMode::Precise, &None) }).await.unwrap();
        assert!(!r.reranked);
        assert!(r.notes.iter().any(|n| n == NOTE_DENIED), "{:?}", r.notes);
        assert_eq!(t.graded(), 0);
        drop(d);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_tenant_project_whose_mirror_is_gone_fails_closed() {
        let (_d, s) = tenant_svc().await;
        s.remember(RememberRequest {
            content: "app note".into(),
            client_project: client_project("k/app", "app", ClientRules::None),
            ..Default::default()
        }).await.unwrap();
        let dir = crate::client_projects::mirror_dir(s.home(), "k/app");
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!s.cloud.cloud_allowed("app"));
        assert!(s.rules_fail_closed(Scope { project: Some("app"), ..Default::default() }).await);
    }
```

`svc_with_grader` builds a local service. That test checks the mirror path, not tenant mode, so it works without `tenant`. If the `cloud` field is private, the test lives in the same module and can use it. If `s.read` is not callable from tests, use `alice_core::store::open_reader(&s.home().db())` instead.

- [ ] **Step 2: Run to see them fail.** Run: `cargo test -p alice-daemon --lib service::tests::a_` and `cargo test -p alice-daemon --lib service::tests::a_mirrored`. Expected: compile errors.

- [ ] **Step 3: Implement.** In `service.rs`:

```rust
/// What a request says about its project (spec §4.1): an explicit slug, the client's working
/// directory (local daemons only) and the project a remote client detected.
#[derive(Debug, Clone, Copy, Default)]
pub struct Scope<'a> {
    pub project: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub client_project: Option<&'a ClientProject>,
}

/// A request that names its project.
pub trait Scoped {
    fn scope(&self) -> Scope<'_>;
}

macro_rules! scoped {
    ($($t:ty),*) => {$(
        impl Scoped for $t {
            fn scope(&self) -> Scope<'_> {
                Scope {
                    project: self.project.as_deref(),
                    cwd: self.cwd.as_deref(),
                    client_project: self.client_project.as_ref(),
                }
            }
        }
    )*};
}
scoped!(RememberRequest, RecallRequest, ContextRequest, IngestRequest);
```

Add to `ServiceOptions`:

```rust
    /// Server mode (`alice daemon run --tenant`, spec §2.2): a request's `cwd` is never used
    /// (no git on a client's path), the settings follow the tenant rules, and a project root
    /// that is not there fails closed.
    pub tenant: bool,
```

Set `tenant: false` in `Default`. In `Service::start`, the settings are loaded (when `opts.settings` is `None`) with:

```rust
        let settings = opts.settings.clone().unwrap_or_else(|| match &opts.settings_env {
            Some(env) => SettingsHandle::load_with(&home, env.clone(), opts.tenant),
            None if opts.tenant => SettingsHandle::load_tenant(&home),
            None => SettingsHandle::load(&home),
        });
```

Replace `choose`:

```rust
    /// Project for a request (precedence: spec §4.1). A `client_project` is mirrored first
    /// (on the blocking pool), whatever wins; a working directory not seen in the last minute is
    /// resolved with git on the blocking pool, and never on a server tenant.
    async fn choose(&self, scope: Scope<'_>) -> Result<ProjectChoice> {
        let mirrored = match scope.client_project {
            Some(cp) => {
                let (home, cp) = (self.home.clone(), cp.clone());
                Some(
                    tokio::task::spawn_blocking(move || crate::client_projects::sync_mirror(&home, &cp))
                        .await
                        .map_err(join_err)??,
                )
            }
            None => None,
        };
        if let Some(slug) = explicit_slug(scope.project) {
            return Ok(ProjectChoice::Slug(slug.to_string()));
        }
        if let Some(r) = mirrored {
            return Ok(ProjectChoice::Detected(r));
        }
        let cwd = if self.opts.tenant { None } else { scope.cwd };
        if let Some(choice) = self.resolver.choose_without_git(None, cwd) {
            return Ok(choice);
        }
        let resolver = self.resolver.clone();
        let cwd = cwd.map(String::from);
        tokio::task::spawn_blocking(move || resolver.choose(None, cwd.as_deref()))
            .await
            .map_err(join_err)
    }

    pub fn is_tenant(&self) -> bool {
        self.opts.tenant
    }
```

At every call site, replace `.choose(req.project.as_deref(), req.cwd.as_deref())` with `.choose(req.scope())`. `rules_fail_closed` becomes:

```rust
    pub async fn rules_fail_closed(&self, scope: Scope<'_>) -> bool {
        let Ok(choice) = self.choose(scope).await else {
            return false;
        };
        // … the rest unchanged
    }
```

In `spool.rs`, replace the `scope` tuple with owned copies, then call the new signature:

```rust
        let (project, cwd, client_project) = match &entry {
            SpoolEntry::Remember(r) => (r.project.clone(), r.cwd.clone(), r.client_project.clone()),
            SpoolEntry::Ingest(r) => (r.project.clone(), r.cwd.clone(), r.client_project.clone()),
        };
        // … later:
                    .rules_fail_closed(crate::service::Scope {
                        project: project.as_deref(),
                        cwd: cwd.as_deref(),
                        client_project: client_project.as_ref(),
                    })
```

- [ ] **Step 4: Run the new tests, then the whole daemon library suite**, since `choose` is central.

Run: `cargo test -p alice-daemon --lib service::tests::a_`, then `cargo test -p alice-daemon --lib`
Expected: PASS (all).

- [ ] **Step 5: Commit**

```bash
git add crates/alice-daemon/src/service.rs crates/alice-daemon/src/spool.rs
git commit -m "feat(daemon): tenant mode in the service — client_project precedence, no git on client paths

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---
### Task 6: HTTP — ping capabilities, structured refusals, tenant refusals, `daemon run --tenant`

**Files:**
- Modify: `crates/alice-daemon/src/http.rs`:
  - `PingResponse` near line 208, `ping` near 228;
  - `ErrorBody` and `refuse` near lines 69–76, `ApiError::into_response` near 162;
  - the `remember`, `ingest` and `make_backup` handlers.
- Modify: `crates/alice-daemon/src/service.rs`:
  - `backup_now` refused in tenant mode;
  - `start_import` refused in tenant mode without a bundle. The bundle half is completed in Task 15. Here, refuse every
    `palace` request when `is_tenant()`.
- Modify: `crates/alice-daemon/src/daemon.rs` (`DaemonOptions` near line 33; `start_and_serve` near lines 217–266)
- Modify: `crates/alice/src/cmd.rs` (`DaemonCmd::Run` becomes `Run { #[arg(long)] tenant: bool }`; dispatch near line 426); `crates/alice/src/main.rs` (`is_server` match)
- Modify: `crates/alice-daemon/src/client.rs` (`error_from` near line 192 rebuilds `AliceError::Refused` from `refused`)
- Test: in-file tests of `http.rs` and `client.rs`; `crates/alice-daemon/tests/http_api.rs` for the wire format

**Interfaces:**
- Consumes: `Scoped` and `Service::is_tenant` (Task 5).
- Produces:
  - `http::CAP_CLIENT_PROJECT = "client-project"`, `http::CAP_TENANT = "tenant"`;
  - `PingResponse.capabilities: Vec<String>`;
  - error body fields `refused: Option<{field, detectors}>` and `retry_later: bool`;
  - `DaemonOptions::tenant: bool`.
  - **Client side:** `error_from` turns a body with `refused` into `AliceError::Refused { field, detectors }`, so
    `alice_core::redact::is_refusal` works across HTTP. `client::refusal_retry_later(&body)` reads `retry_later` for the
    replay (Task 11).

**Wire format.**
- `{"error": "...", "kind": "invalid", "refused": {"field": "content", "detectors": ["github_token"]}, "retry_later": true}`.
- `refused` and `retry_later` are skipped when absent or false; `kind` stays `"invalid"` for compatibility.
- `retry_later` is computed in the `remember` and `ingest` handlers. On an `AliceError::Refused`, call
  `st.svc.rules_fail_closed(req.scope()).await`, using a scope taken from a clone of the request made before it was moved.

For the client, keep one decoding rule: `error_from` returns `AliceError::Refused { field, detectors }` when `refused` is
present. Add a public helper `client::refusal_retry_later(body: &serde_json::Value) -> bool`, and make the low-level
`DaemonClient::send_raw` (Task 9) expose the body, so the replay (Task 11) can read `retry_later`. Here, only add
`error_from`'s `Refused` mapping and the helper.

- [ ] **Step 1: Write the failing tests.** In `http.rs` tests, using the existing router helpers there (look at
  `a_lagging_client_is_told_how_many_items_it_missed` for how a router is built):

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn the_ping_lists_capabilities_and_a_tenant_says_so() {
        for tenant in [false, true] {
            let d = tempfile::tempdir().unwrap();
            let svc = Service::start(
                AliceHome::new(d.path().to_path_buf()),
                &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>),
                ServiceOptions { tenant, ..ServiceOptions::default() },
            ).unwrap();
            let app = router(svc.clone(), "t".repeat(64), 47801, CancellationToken::new());
            let resp = tower::ServiceExt::oneshot(app, axum::http::Request::builder()
                .uri("/api/v1/ping").header("host", "127.0.0.1:47801")
                .body(axum::body::Body::empty()).unwrap()).await.unwrap();
            let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
            let p: PingResponse = serde_json::from_slice(&body).unwrap();
            assert!(p.capabilities.contains(&CAP_CLIENT_PROJECT.to_string()));
            assert_eq!(p.capabilities.contains(&CAP_TENANT.to_string()), tenant);
        }
    }

    #[test]
    fn a_refusal_carries_its_field_and_detectors() {
        let r = ApiError(AliceError::Refused { field: "content".into(), detectors: vec!["github_token".into()] }).into_response();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let body = futures::executor::block_on(axum::body::to_bytes(r.into_body(), 1 << 20)).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["kind"], "invalid");
        assert_eq!(v["refused"]["field"], "content");
        assert_eq!(v["refused"]["detectors"][0], "github_token");
        assert!(v.get("retry_later").is_none());
    }
```

`tower` may not be a direct dependency. If so, use the pattern the existing http tests use to call the router; for
example, serve it on `127.0.0.1:0` and call it with `reqwest`. In `client.rs` tests:

```rust
    #[test]
    fn a_refusal_body_becomes_a_refusal_error() {
        let body = serde_json::json!({"error": "the content contains …", "kind": "invalid",
            "refused": {"field": "content", "detectors": ["github_token"]}, "retry_later": true});
        let e = error_from(reqwest::StatusCode::BAD_REQUEST, &body);
        assert!(alice_core::redact::is_refusal(&e), "{e}");
        assert!(refusal_retry_later(&body));
        assert!(!refusal_retry_later(&serde_json::json!({"error": "x", "kind": "invalid"})));
    }
```

In `service.rs` tests (tenant refusals):

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tenant_refuses_manual_backups_and_server_palace_imports() {
        let (_d, s) = tenant_svc().await;
        let e = s.backup_now().await.unwrap_err().to_string();
        assert!(e.contains("backups are scheduled"), "{e}");
        let e = s.start_import(alice_core::import::ImportRequest { palace: Some("/srv/x".into()), ..Default::default() }).unwrap_err().to_string();
        assert!(e.contains("server tenant"), "{e}");
        let e = s.start_import(alice_core::import::ImportRequest::default()).unwrap_err().to_string();
        assert!(e.contains("server tenant"), "{e}");
    }
```

- [ ] **Step 2: Run to see them fail.**

Run: `cargo test -p alice-daemon --lib the_ping_lists`, `cargo test -p alice-daemon --lib a_refusal`, `cargo test -p alice-daemon --lib a_tenant_refuses`
Expected: compile errors.

- [ ] **Step 3: Implement.**
  - **`PingResponse`:** add `#[serde(default, skip_serializing_if = "Vec::is_empty")] pub capabilities: Vec<String>`. In
    `ping`, fill it with `CAP_CLIENT_PROJECT`, plus `CAP_TENANT` when `st.svc.is_tenant()`.
  - **`ErrorBody`:**

```rust
#[derive(Debug, Serialize)]
struct RefusedBody {
    field: String,
    detectors: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    refused: Option<RefusedBody>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    retry_later: bool,
}
```

  - **`refuse`** fills `refused: None, retry_later: false`.
  - **`ApiError::into_response`** fills `refused` for `AliceError::Refused { field, detectors }`.
  - **Retry helper:** add
    `fn refusal_response(e: AliceError, retry_later: bool) -> Response` (`BAD_REQUEST`, kind `invalid`, `refused` filled).
    In `remember`, keep `let scope_req = req.clone();` before calling `st.svc.remember_or_spool(req)`. On
    `Err(e) if alice_core::redact::is_refusal(&e)`, return
    `refusal_response(e, st.svc.rules_fail_closed(scope_req.scope()).await)`. Do the same in `ingest`.
  - **`Service::backup_now`:** at the top, when `self.opts.tenant`, return
    `Err(AliceError::invalid("on a server tenant backups are scheduled (the last 2 are kept); ask the server's admin for an extra copy"))`.
  - **`Service::start_import`:** when `self.opts.tenant` and `req.bundle.is_none()` (the field arrives in Task 13; until
    then, test `true` and leave a comment pointing at Task 15), return
    `Err(AliceError::invalid("a server tenant imports only a bundle uploaded by `alice import mempalace` from your own machine; it cannot read a palace on the server"))`.
    Until Task 13 adds `bundle`, refuse every import in tenant mode with that message.
  - **`DaemonOptions::tenant`.** In `start_and_serve`, load settings with
    `SettingsHandle::load_with(home, opts.settings_env.clone().unwrap_or_else(crate::config::process_env), opts.tenant)`.
    Make `process_env` `pub` if it is not. Pass `tenant: opts.tenant` in the `ServiceOptions`, and log
    `tracing::info!(tenant = opts.tenant, "starting")` once.
  - **CLI:** `DaemonCmd::Run { #[arg(long, help = "Serve one person on a shared server (alice-server's units use it)")] tenant: bool }`.
    Dispatch passes `tenant`. Update every `DaemonCmd::Run` match to `DaemonCmd::Run { .. }`.
  - **`client.rs`:**

```rust
/// Whether a refusal answer says the project's rules were fail-closed when it was made, so the
/// same write may be accepted later (a replayed spool entry is kept, not withheld).
pub fn refusal_retry_later(body: &serde_json::Value) -> bool {
    body["retry_later"].as_bool() == Some(true)
}
```

  In `error_from`, before matching `kind`:

```rust
    if let Some(r) = body.get("refused").filter(|r| r.is_object()) {
        return AliceError::Refused {
            field: r["field"].as_str().unwrap_or("text").to_string(),
            detectors: r["detectors"].as_array().into_iter().flatten()
                .filter_map(|d| d.as_str().map(String::from)).collect(),
        };
    }
```

- [ ] **Step 4: Run the tests to see them pass**, then the daemon and CLI suites that exercise errors.

Run the three filters above, then `cargo test -p alice-daemon --test http_api`, then `cargo test -p alice-memory --test cli -- --test-threads 2`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/alice-daemon/src/http.rs crates/alice-daemon/src/service.rs crates/alice-daemon/src/daemon.rs crates/alice-daemon/src/client.rs crates/alice-daemon/src/config.rs crates/alice/src/cmd.rs crates/alice/src/main.rs
git commit -m "feat(daemon): ping capabilities, structured refusals with retry_later, tenant refusals, daemon run --tenant

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 7: `SpoolSink` — one drain for the daemon and the remote client

**Files:**
- Modify: `crates/alice-daemon/src/spool.rs` (`drain` near line 128)
- Modify: `crates/alice-daemon/src/service.rs` (`impl SpoolSink for Service`)
- Modify: `crates/alice-daemon/src/daemon.rs` (`drain_spool` calls `spool::drain(&dir, svc.as_ref())`)
- Test: the existing spool tests stay green; add one test with a scripted sink

**Interfaces:**
- Produces:

```rust
/// What happened to one spooled entry.
pub enum Applied {
    /// Stored (or already held).
    Done,
    /// Block mode refused it. `retry_later`: the project's rules were fail-closed, so keep it.
    Refused { retry_later: bool, why: String },
    /// It will never be accepted (invalid, unknown project or id): move it to `rejected/`.
    Rejected(String),
    /// The target is away or failing: stop, keep the order.
    Paused(String),
}

/// Where spooled entries are applied: the daemon's own service, or a remote tenant (Task 11).
pub trait SpoolSink {
    fn apply(&self, entry: SpoolEntry) -> impl std::future::Future<Output = Applied> + Send;
}

pub async fn drain<S: SpoolSink + Sync>(dir: &Path, sink: &S) -> DrainReport
```

- [ ] **Step 1: Write the failing test** (in `spool.rs` tests):

```rust
    struct Scripted(std::sync::Mutex<Vec<Applied>>, std::sync::Mutex<Vec<String>>);

    impl SpoolSink for Scripted {
        async fn apply(&self, entry: SpoolEntry) -> Applied {
            if let SpoolEntry::Remember(r) = &entry {
                self.1.lock().unwrap().push(r.content.clone());
            }
            self.0.lock().unwrap().remove(0)
        }
    }

    #[tokio::test]
    async fn the_drain_follows_what_the_sink_says() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("spool");
        for c in ["a", "b", "c", "d", "e"] {
            append(&dir, &remember(c)).unwrap();
        }
        let sink = Scripted(
            std::sync::Mutex::new(vec![
                Applied::Done,
                Applied::Refused { retry_later: true, why: "fail-closed".into() },
                Applied::Refused { retry_later: false, why: "secret".into() },
                Applied::Rejected("bad".into()),
                Applied::Paused("away".into()),
            ]),
            Default::default(),
        );
        let r = drain(&dir, &sink).await;
        assert_eq!(*sink.1.lock().unwrap(), vec!["a", "b", "c", "d", "e"]);
        assert_eq!(r, DrainReport { processed: 1, rejected: 2, remaining: 2 });
        // b (kept for later) and e (paused) remain; c was withheld, d moved to rejected/.
        assert_eq!(pending_count(&dir), 2);
        assert!(dir.join("rejected").read_dir().unwrap().count() >= 2);
    }
```

- [ ] **Step 2: Run to see it fail.** Run: `cargo test -p alice-daemon --lib spool::tests::the_drain_follows`. Expected: compile error (`SpoolSink` missing).

- [ ] **Step 3: Implement.** Rewrite the loop body of `drain` around `sink.apply(entry).await`:
  - `Applied::Done`: remove the file, `processed += 1`.
  - `Applied::Refused { retry_later: true, .. }`: report it once (the existing `REPORTED` set and warning text),
    `remaining += 1`, continue.
  - `Applied::Refused { retry_later: false, why }`: `withhold(dir, file, &why)`, `rejected += 1`.
  - `Applied::Rejected(why)`: `reject(dir, file, &why)`, `rejected += 1`.
  - `Applied::Paused(why)`: log the warning `"spool drain paused"` with `why`, set `remaining = files.len() - i`, return.
  - A file that does not parse keeps today's `reject`.

`impl SpoolSink for Service` (in `service.rs`) moves today's logic:

```rust
impl spool::SpoolSink for Service {
    async fn apply(&self, entry: spool::SpoolEntry) -> spool::Applied {
        use spool::{Applied, SpoolEntry};
        let (project, cwd, client_project) = match &entry {
            SpoolEntry::Remember(r) => (r.project.clone(), r.cwd.clone(), r.client_project.clone()),
            SpoolEntry::Ingest(r) => (r.project.clone(), r.cwd.clone(), r.client_project.clone()),
        };
        let result = match entry {
            SpoolEntry::Remember(r) => self.remember(r).await.map(|_| ()),
            SpoolEntry::Ingest(r) => self.ingest(r).await.map(|_| ()),
        };
        match result {
            Ok(()) => Applied::Done,
            Err(e) if alice_core::redact::is_refusal(&e) => {
                let scope = Scope { project: project.as_deref(), cwd: cwd.as_deref(), client_project: client_project.as_ref() };
                Applied::Refused { retry_later: self.rules_fail_closed(scope).await, why: e.to_string() }
            }
            Err(e @ (AliceError::Invalid(_) | AliceError::NotFound(_))) => Applied::Rejected(e.to_string()),
            Err(e) => Applied::Paused(e.to_string()),
        }
    }
}
```

Update the callers (`daemon.rs`, the tests in `client.rs` and `service.rs` that call `spool::drain(&dir, &svc)`) to pass
`svc.as_ref()` or `&*svc`.

- [ ] **Step 4: Run all spool and service tests.** Run: `cargo test -p alice-daemon --lib spool::`, then
  `cargo test -p alice-daemon --lib service::`, then `cargo test -p alice-daemon --lib client::`. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/alice-daemon/src/spool.rs crates/alice-daemon/src/service.rs crates/alice-daemon/src/daemon.rs crates/alice-daemon/src/client.rs
git commit -m "refactor(spool): drain through a SpoolSink, so a remote client can replay (behaviour unchanged)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 8: Remote configuration — `remote_config.rs`

**Files:**
- Create: `crates/alice-daemon/src/remote_config.rs`
- Modify: `crates/alice-daemon/src/lib.rs` (`pub mod remote_config;`), `crates/alice-daemon/src/paths.rs` (`remote_file()`)
- Test: in-file tests

**Interfaces:**
- Produces:

```rust
pub const ENV_URL: &str = "ALICE_REMOTE_URL";
pub const ENV_TOKEN: &str = "ALICE_REMOTE_TOKEN";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigSource { File, Env }

#[derive(Clone)]
pub struct RemoteConfig { pub url: String, pub token: String, pub source: ConfigSource }
// Debug prints url and source, never the token.

/// `https://host[:port][/path]` (or `http://` to loopback), normalized: lower-case scheme and
/// host, no trailing `/`. Refuses user info, a query, a fragment, other schemes, an empty host.
pub fn normalize_url(raw: &str) -> Result<String, String>;
/// The environment (both variables, or neither), else `remote.json`, else `None`.
pub fn load(home: &AliceHome) -> Result<Option<RemoteConfig>, String>;
pub fn load_with(home: &AliceHome, env: &dyn Fn(&str) -> Option<String>) -> Result<Option<RemoteConfig>, String>;
/// Written private (0600) and atomically; `url` must already be normalized.
pub fn save(home: &AliceHome, url: &str, token: &str) -> std::io::Result<()>;
/// `true` when there was a file to remove.
pub fn remove(home: &AliceHome) -> std::io::Result<bool>;
```

`AliceHome::remote_file()` returns `<root>/remote.json`. The token must look like a token: 16–256 characters, ASCII and
not whitespace. The check refuses without echoing the value.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> (tempfile::TempDir, AliceHome) {
        let d = tempfile::tempdir().unwrap();
        let h = AliceHome::new(d.path().to_path_buf());
        h.ensure_dirs().unwrap();
        (d, h)
    }

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn urls_are_normalized_and_refused_by_the_spec_rules() {
        assert_eq!(normalize_url("HTTPS://203.0.113.7:8443/u/ann/").unwrap(), "https://203.0.113.7:8443/u/ann");
        assert_eq!(normalize_url("https://Example.COM").unwrap(), "https://example.com");
        assert_eq!(normalize_url(" http://127.0.0.1:47801/u/t/ ").unwrap(), "http://127.0.0.1:47801/u/t");
        assert_eq!(normalize_url("http://localhost:9").unwrap(), "http://localhost:9");
        assert_eq!(normalize_url("http://[::1]:9/x").unwrap(), "http://[::1]:9/x");
        for bad in ["http://203.0.113.7/u/ann", "ftp://x", "https://", "https://u:p@host/x",
                    "https://host/x?y=1", "https://host/x#f", "host/u/ann", ""] {
            assert!(normalize_url(bad).is_err(), "{bad}");
        }
        assert!(!normalize_url("https://u:p@host/x").unwrap_err().contains("u:p"), "user info not echoed");
    }

    #[test]
    fn the_file_is_private_and_the_environment_wins() {
        let (_d, h) = home();
        assert!(load_with(&h, &|_| None).unwrap().is_none());
        save(&h, "https://203.0.113.7/u/ann", TOKEN).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(h.remote_file()).unwrap().permissions().mode() & 0o777, 0o600);
            std::fs::set_permissions(h.remote_file(), std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let c = load_with(&h, &|_| None).unwrap().unwrap();
        assert_eq!((c.url.as_str(), c.token.as_str(), c.source), ("https://203.0.113.7/u/ann", TOKEN, ConfigSource::File));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(h.remote_file()).unwrap().permissions().mode() & 0o777, 0o600, "made private again");
        }
        let env = |k: &str| match k {
            ENV_URL => Some("https://other.example/u/bob/".to_string()),
            ENV_TOKEN => Some(TOKEN.to_string()),
            _ => None,
        };
        let c = load_with(&h, &env).unwrap().unwrap();
        assert_eq!((c.url.as_str(), c.source), ("https://other.example/u/bob", ConfigSource::Env));
        let half = |k: &str| (k == ENV_URL).then(|| "https://x.example".to_string());
        let e = load_with(&h, &half).unwrap_err();
        assert!(e.contains(ENV_TOKEN), "{e}");
        assert!(remove(&h).unwrap());
        assert!(!remove(&h).unwrap());
        assert!(load_with(&h, &|_| None).unwrap().is_none());
    }

    #[test]
    fn the_token_never_shows() {
        let c = RemoteConfig { url: "https://h/u/a".into(), token: TOKEN.into(), source: ConfigSource::File };
        assert!(!format!("{c:?}").contains(TOKEN));
        let (_d, h) = home();
        std::fs::write(h.remote_file(), format!("{{\"url\": \"ftp://bad\", \"token\": \"{TOKEN}\"}}")).unwrap();
        let e = load_with(&h, &|_| None).unwrap_err();
        assert!(!e.contains(TOKEN), "{e}");
        std::fs::write(h.remote_file(), b"{not json").unwrap();
        assert!(load_with(&h, &|_| None).is_err());
    }
}
```

- [ ] **Step 2: Run to see them fail.** Run: `cargo test -p alice-daemon --lib remote_config`. Expected: compile errors.

- [ ] **Step 3: Implement.**
  - Use `url::Url` to parse. Check that the scheme is `https`, or `http` with a loopback host (`127.0.0.1`, `::1`,
    `localhost`). Check `username().is_empty() && password().is_none()`, `query().is_none()`, `fragment().is_none()`, and
    a host. Rebuild the string as `format!("{scheme}://{host_str_with_brackets}{port_part}{path_trimmed}")`, where
    `path_trimmed` is the path without its trailing `/`, and empty for `/`.
  - `load_with`:
    - read both variables, treating empty as unset;
    - both set: normalize the URL, check the token, return `Env`;
    - one set: `Err(format!("set both {ENV_URL} and {ENV_TOKEN}, or neither ({ENV_TOKEN} is missing)"))`, naming the
      missing one;
    - neither: read `remote.json` (missing means `Ok(None)`), set the mode to 0600 on Unix (failure gives an error naming
      the file), parse `{"url", "token"}` with serde, normalize and check, return `File`.
  - **Every error message is about the field**, never the value.
  - `save` writes JSON `{"url": .., "token": ..}` through `create_new` with mode 0600, `sync_all`, then `rename` (the
    pattern of `discovery::write_private` plus `rotate_token_with`). On failure it removes the temporary file.
  - `Debug` is a manual implementation that prints `url` and `source` and calls `finish_non_exhaustive()`.

- [ ] **Step 4: Run to see them pass.** Run: `cargo test -p alice-daemon --lib remote_config`. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/alice-daemon/src/remote_config.rs crates/alice-daemon/src/lib.rs crates/alice-daemon/src/paths.rs
git commit -m "feat(client): remote configuration — remote.json (0600) or ALICE_REMOTE_URL/TOKEN, URL rules

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 9: The remote target in `DaemonClient`, and the Caddy emulator harness

**Files:**
- Modify: `crates/alice-daemon/src/client.rs` (struct `DaemonClient`, `Unavailable`, `http_client`, `proves`, `try_connect`, `try_existing`, `is_live`, `call`, `get_json`, every method's timeout)
- Create: `crates/alice-daemon/tests/support/mod.rs`, `crates/alice-daemon/tests/support/caddy.rs`
- Create: `crates/alice-daemon/tests/server_mode.rs`
- Test: `server_mode.rs` (integration), plus unit tests in `client.rs`

**Interfaces:**
- Consumes: `remote_config::{load, RemoteConfig}` (Task 8); `PingResponse.capabilities` and `CAP_CLIENT_PROJECT` (Task 6).
- Produces:
  - `DaemonClient::is_remote(&self) -> bool`;
  - `DaemonClient::base(&self) -> &str`;
  - `DaemonClient::server_ping(&self) -> Result<PingResponse>` (a fresh ping with proof, for `alice remote status`);
  - `DaemonClient::connect_with(home: &AliceHome, cfg: &RemoteConfig) -> Result<DaemonClient, Unavailable>` (connect with
    a configuration that is not saved yet; `alice remote set` uses it, and `try_connect` calls it after `remote_config::load`);
  - `DaemonClient::send_raw(&self, method: reqwest::Method, path: &str, body: Option<&serde_json::Value>, timeout: Duration) -> Result<(reqwest::StatusCode, serde_json::Value)>` (used by the replay);
  - new `Unavailable` variants:

```rust
    /// Remote mode: the configuration is invalid (`remote.json` or the environment).
    RemoteConfig(String),
    /// Remote mode: the server did not answer (network, TLS, 5xx, a timeout).
    RemoteUnreachable { url: String, error: String },
    /// Remote mode: something answered but did not prove it holds this client's token.
    RemoteUnproven { url: String },
    /// Remote mode: the server runs an Alice without client-project support.
    RemoteTooOld { url: String },
```

  Display (each starts with `UNAVAILABLE`, "memory daemon "):
  - `RemoteConfig`: `memory daemon settings for remote mode are invalid: {why}`
  - `RemoteUnreachable`: `memory daemon at {url} is unreachable ({error}); writes are queued and sent when it answers`
  - `RemoteUnproven`: `memory daemon at {url} did not prove it holds this client's token; check the URL (…/u/<name>) and run "alice remote set" with the token from the server's admin`
  - `RemoteTooOld`: `memory daemon at {url} runs an older Alice without client-project support; ask the server's admin to upgrade it ("alice-server upgrade")`

  `queued_note` for each remote variant: `the memory server {url} is not reachable; the memory is sent when it is`. For
  `RemoteConfig`: `remote mode is misconfigured ({why}); the memory is sent once "alice remote status" is happy`.

**Behaviour** (spec §3.2):
- **Choosing the target.** `try_connect`, `try_existing` and `connect` first call `remote_config::load(home)`:
  - `Err(why)` gives `Unavailable::RemoteConfig(why)` (`try_existing` gives `None`);
  - `Ok(Some(cfg))` gives `connect_remote(home, cfg)`;
  - `Ok(None)` keeps today's local path.
  - A remote target never reads `daemon.json`, never runs the launcher, and never looks at the restore marker.
- **`connect_remote`:**
  - builds `remote_http_client()`: `connect_timeout` 5 s; overall default timeout 30 s; `Policy::none()` redirects;
    user agent `alice/<version>`; system proxy honoured for `https://`; `.no_proxy()` for `http://` loopback;
  - sends `GET {base}/api/v1/ping?nonce=<fresh>` with a 3 s timeout and no token;
  - a transport error, or a 502, 503 or 504, gives `RemoteUnreachable`;
  - any other status, or a body that is not a `PingResponse` (HTML from Caddy, empty), gives `RemoteUnproven`;
  - checks `service == "alice"` and `verify_ping_proof(token, nonce, &ping.instance, proof)`, else `RemoteUnproven`;
  - `capabilities` must contain `client-project`, else `RemoteTooOld`;
  - on success returns `DaemonClient { http, base, token, instance: ping.instance, remote: Some(Arc<RemoteState>) }`, where
    `RemoteState { home: AliceHome, healthy: AtomicBool, projects: crate::remote_prepare::ProjectCache }`. Until Task 10
    adds `ProjectCache`, `RemoteState` carries `home` and `healthy` only, and Task 10 adds the field.
- **`is_live`:** remote returns `healthy.load()`, with no network (R9). Local is unchanged.
- **`call`, `get_json` and the other request methods:**
  - on a transport error or a 401 in remote mode, set `healthy` to false;
  - the timeout of each method is chosen by a helper `fn t(&self, local: Duration, remote: Duration) -> Duration`, with the
    remote column of the Global Constraints table;
  - `get_json` gets a timeout too (remote 15 s; local keeps the client default).
- **401 in remote mode** maps to `AliceError::Other(Unavailable::RemoteUnproven{url}.to_string())`, so writes spool and
  reads say what to do. Local mode keeps `error_from`'s text.

**The Caddy emulator** (`tests/support/caddy.rs`):

```rust
//! Two in-process tenant daemons behind an emulator of the server's Caddy (spec §2.3): `/u/<name>/…` has the prefix
//! stripped and goes to that tenant's router with `Host: 127.0.0.1:<its port>`; `/u/<name>/api/v1/admin/*` and
//! anything outside `/u/<name>/api/v1/*` answer 404 with an HTML body, as Caddy would.
pub struct Tenant { pub name: String, pub home: AliceHome, pub svc: Arc<Service>, pub token: String, pub port: u16 }
pub struct Server { pub base: String, pub tenants: Vec<Tenant>, stop: CancellationToken, _dirs: Vec<tempfile::TempDir> }
impl Server {
    /// Starts tenants `names` (tenant mode on, fake embedder) behind one emulator on 127.0.0.1:0.
    pub async fn start(names: &[&str]) -> Server;
    pub fn url(&self, name: &str) -> String; // "{base}/u/{name}"
    pub fn tenant(&self, name: &str) -> &Tenant;
    /// Every request the emulator received for `name`: (method, path, authorization header present, body bytes).
    pub fn seen(&self, name: &str) -> Vec<(String, String, bool, Vec<u8>)>;
    /// Stop answering (connections refused) until `resume`.
    pub async fn pause(&self); pub async fn resume(&self);
}
impl Drop for Server { fn drop(&mut self) { self.stop.cancel(); for t in &self.tenants { t.svc.shutdown(); } } }
```

**Implementation of the emulator:**
- **Per tenant:** `alice_daemon::discovery::load_or_create_token(&home)`, then
  `Service::start(home, fake, ServiceOptions { tenant: true, ..Default::default() })`, then
  `alice_daemon::http::router(svc, token, port, stop)`, where `port` is a fixed fake number per tenant (47801, 47802, …).
- **Recorder:** an outer `axum::middleware::from_fn` records each request (buffer the body with
  `axum::body::to_bytes(body, usize::MAX)` and rebuild the request) and sets `Host: 127.0.0.1:<port>`.
- **Routing:** `Router::new().nest(&format!("/u/{name}"), tenant_router)` for each tenant.
- **Refusals:** a first middleware answers 404 with `text/html` (`<html><body>404</body></html>`) for an admin path, and
  for any path outside `/u/<known>/api/v1/`.
- **`pause`:** stop the listener, keeping the port. The simple way is a flag the middleware checks, answering 502
  `text/html`, which is what Caddy sends for a tenant that is down. Also provide `kill_listener()` for a refused
  connection.

**Tests** in `tests/server_mode.rs`:

```rust
mod support;
use support::caddy::Server;
use alice_daemon::client::{DaemonClient, Launcher, Unavailable};
use alice_daemon::paths::AliceHome;
use std::time::Duration;

fn client_home() -> (tempfile::TempDir, AliceHome) {
    let d = tempfile::tempdir().unwrap();
    let h = AliceHome::new(d.path().to_path_buf());
    h.ensure_dirs().unwrap();
    (d, h)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_client_proves_the_tenant_then_calls_it() {
    alice_daemon::install_tls();
    let srv = Server::start(&["ann", "bob"]).await;
    let (_d, home) = client_home();
    alice_daemon::remote_config::save(&home, &srv.url("ann"), &srv.tenant("ann").token).unwrap();
    // A daemon.json pointing nowhere and a launcher that must never run.
    std::fs::write(home.daemon_json(), br#"{"pid":1,"port":1,"instance":"x","version":"t","started_at":"t"}"#).unwrap();
    let c = DaemonClient::try_connect(&home, &Launcher::Exe("/nonexistent/alice".into()), Duration::from_millis(100)).await.unwrap();
    assert!(c.is_remote());
    assert_eq!(c.health().await.unwrap().instance, srv.tenant("ann").svc.instance());
    let seen = srv.seen("ann");
    assert!(seen[0].1.ends_with("/api/v1/ping") && !seen[0].2, "the first request is a ping without the token");
    assert!(seen.iter().skip(1).all(|r| r.2), "later requests carry it");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_never_reaches_the_wrong_tenant() {
    alice_daemon::install_tls();
    let srv = Server::start(&["ann", "bob"]).await;
    let (_d, home) = client_home();
    alice_daemon::remote_config::save(&home, &srv.url("bob"), &srv.tenant("ann").token).unwrap();
    let e = DaemonClient::try_connect(&home, &Launcher::None, Duration::ZERO).await.unwrap_err();
    assert!(matches!(e, Unavailable::RemoteUnproven { .. }), "{e}");
    assert!(srv.seen("bob").iter().all(|r| !r.2), "no request to bob carried a token");
    let r = alice_daemon::client::remember_or_spool(&home, &Launcher::None,
        alice_core::api::RememberRequest { content: "keep me".into(), ..Default::default() }).await.unwrap();
    assert_eq!(r.status, alice_core::api::RememberStatus::Spooled);
}

#[tokio::test(flavor = "multi_thread")]
async fn html_and_missing_tenants_read_as_unproven_or_unreachable_never_as_decode_errors() {
    alice_daemon::install_tls();
    let srv = Server::start(&["ann"]).await;
    let (_d, home) = client_home();
    alice_daemon::remote_config::save(&home, &format!("{}/u/nobody", srv.base), &srv.tenant("ann").token).unwrap();
    let e = DaemonClient::try_connect(&home, &Launcher::None, Duration::ZERO).await.unwrap_err();
    assert!(matches!(e, Unavailable::RemoteUnproven { .. }), "{e}");
    srv.pause().await;
    alice_daemon::remote_config::save(&home, &srv.url("ann"), &srv.tenant("ann").token).unwrap();
    let e = DaemonClient::try_connect(&home, &Launcher::None, Duration::ZERO).await.unwrap_err();
    assert!(matches!(e, Unavailable::RemoteUnreachable { .. }), "{e}");
    assert!(!e.to_string().contains(&srv.tenant("ann").token));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_old_server_is_refused_before_the_token_is_sent() {
    // The emulator's `start_without_capabilities` serves a ping that proves the token but lists no capabilities.
    alice_daemon::install_tls();
    let srv = Server::start_without_capabilities("ann").await;
    let (_d, home) = client_home();
    alice_daemon::remote_config::save(&home, &srv.url("ann"), &srv.tenant("ann").token).unwrap();
    let e = DaemonClient::try_connect(&home, &Launcher::None, Duration::ZERO).await.unwrap_err();
    assert!(matches!(e, Unavailable::RemoteTooOld { .. }), "{e}");
    assert!(srv.seen("ann").iter().all(|r| !r.2));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_ping_is_fine_remotely() {
    alice_daemon::install_tls();
    let srv = Server::start(&["ann"]).await;
    srv.delay_ping(Duration::from_millis(1000));
    let (_d, home) = client_home();
    alice_daemon::remote_config::save(&home, &srv.url("ann"), &srv.tenant("ann").token).unwrap();
    assert!(DaemonClient::try_connect(&home, &Launcher::None, Duration::ZERO).await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn server_sent_events_pass_the_emulator_unbuffered() {
    alice_daemon::install_tls();
    let srv = Server::start(&["ann"]).await;
    let t = srv.tenant("ann");
    let http = reqwest::Client::new();
    let mut resp = http.get(format!("{}/api/v1/events/stream", srv.url("ann"))).bearer_auth(&t.token).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let first = tokio::time::timeout(Duration::from_secs(2), resp.chunk()).await.unwrap().unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("data:"), "the health event arrives at once");
    t.svc.remember(alice_core::api::RememberRequest { content: "an event".into(), ..Default::default() }).await.unwrap();
    let next = tokio::time::timeout(Duration::from_secs(1), resp.chunk()).await.unwrap().unwrap().unwrap();
    assert!(String::from_utf8_lossy(&next).contains("data:"));
}
```

Add `Server::start_without_capabilities(name)`: a tenant whose emulated ping endpoint answers
`{"service":"alice","instance":<svc.instance()>,"version":"0.0.9","proof": <ping_proof(token, nonce, instance)>}` with no
`capabilities`, overriding the daemon's ping. Add `Server::delay_ping(d)`: the middleware sleeps `d` before forwarding a
ping.

- [ ] **Step 1: Write the support module and the tests above.** The test file fails to compile until the client gains the
  remote target.
- [ ] **Step 2: Run to see them fail.** Run: `cargo test -p alice-daemon --test server_mode`. Expected: compile errors in
  the use of `is_remote`, `Unavailable::RemoteUnproven` and the others.
- [ ] **Step 3: Implement the client changes** described under Behaviour. Keep the local path byte-for-byte the same (its
  tests must stay green). Implementation notes:
  - `proves_remote(http, base, token) -> Result<PingResponse, Unavailable>` sits next to `proves`;
  - `DaemonClient` gains `remote: Option<Arc<RemoteState>>`;
  - `fn mark_unhealthy(&self)`.
- [ ] **Step 4: Run the new tests and the client unit tests.** Run: `cargo test -p alice-daemon --test server_mode`, then
  `cargo test -p alice-daemon --lib client::`. Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add crates/alice-daemon/src/client.rs crates/alice-daemon/tests/support/mod.rs crates/alice-daemon/tests/support/caddy.rs crates/alice-daemon/tests/server_mode.rs
git commit -m "feat(client): the remote target — proof before token, capabilities, WAN timeouts, Caddy emulator tests

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 10: Client-side preparation — project identity and redaction (`remote_prepare.rs`)

**Files:**
- Create: `crates/alice-daemon/src/remote_prepare.rs`
- Modify: `crates/alice-daemon/src/lib.rs`, `crates/alice-daemon/src/client.rs` (`RemoteState.projects`; `recall`, `context`, `remember`, `ingest`, `update`, `assert_fact`, `compat_call` prepare in remote mode; `remember_via` and `ingest_or_spool` spool prepared requests in remote mode)
- Modify: `crates/alice-daemon/src/compat.rs` (`CompatCall.client_project`, passed into the four requests it builds at the `cwd: call.cwd.clone()` sites) and `crates/alice-daemon/src/service.rs` if compat reaches `choose` through those requests (it does: no further change)
- Test: unit tests in `remote_prepare.rs`; integration tests in `tests/server_mode.rs`

**Interfaces:**
- Consumes: `ClientProject` and `ClientRules` (Task 1); `remote_config` (Task 8); the remote `DaemonClient` (Task 9).
- Produces:

```rust
/// Per-process cache of working directory -> detected project (60 s, like the daemon's resolver).
#[derive(Default)] pub struct ProjectCache { /* Mutex<HashMap<String, (Option<ProjectRef>, Instant)>> */ }

/// The project `cwd` belongs to and its working tree's rules, as a remote client sends them;
/// `None` for `global` (home, root, a vanished directory). Runs git: blocking.
pub fn client_project(cache: &ProjectCache, cwd: &str) -> Option<(ClientProject, Option<PathBuf>)>;

/// The redactor for text leaving this machine (spec §3.4): this machine's global privacy
/// (`config.toml` and `ALICE_*`, fail closed when degraded), the project's rules on top when known,
/// `privacy.redact` forced on.
pub fn redactor_for(home: &AliceHome, rules: Option<&ClientRules>) -> Redactor;

pub fn prepare_remember(home: &AliceHome, cache: &ProjectCache, req: RememberRequest) -> Result<RememberRequest>;
pub fn prepare_ingest(home: &AliceHome, cache: &ProjectCache, req: IngestRequest) -> Result<IngestRequest>;
pub fn prepare_recall(home: &AliceHome, cache: &ProjectCache, req: RecallRequest) -> RecallRequest;
pub fn prepare_context(cache: &ProjectCache, req: ContextRequest) -> ContextRequest;
pub fn prepare_update(home: &AliceHome, cache: &ProjectCache, req: UpdateRequest) -> Result<UpdateRequest>;
pub fn prepare_fact(home: &AliceHome, cache: &ProjectCache, req: AssertFactRequest) -> Result<AssertFactRequest>;
pub fn prepare_compat(home: &AliceHome, cache: &ProjectCache, call: CompatCall) -> CompatCall;
```

**Rules** (spec §3.3, §3.4):
- **Identity.**
  - Every `prepare_*` that has a `cwd`, and whose request names no explicit `project`, resolves the identity and sets
    `client_project`.
  - When the request already has `client_project` (a replayed entry, a second call), it is kept as is: this makes the
    functions idempotent.
  - `cwd` is always set to `None`.
  - `update`, `assert_fact` and compat use the **process** working directory for the redaction rules
    (`std::env::current_dir()`), as they carry no `cwd`. Compat's own `cwd` field is used when present.
- **Reading the rules file** at `<root>/.alice.toml`:
  - absent → `ClientRules::None`;
  - a regular file of up to 64 KiB, UTF-8 → `File { text }`;
  - anything else (a directory, larger than 64 KiB, not UTF-8, a read error, a symlink that leads outside `root`) →
    `Unreadable { reason }` with a short reason (`"is a directory"`, `"is larger than 64 KiB"`, `"is not UTF-8"`,
    `"permission denied"`, `"is a link outside the project"`).
- **Redaction of the text fields:**

  | Request | Fields | How |
  |---|---|---|
  | `remember` | content, title, topics | `check` |
  | `ingest` | source uri, chunk texts | `check` |
  | `update` | title, body, topics | `check` |
  | `fact` | subject, predicate, object | `check` |
  | `recall` | query | `apply(text).text` |
  | `compat` | every string inside `arguments` | `Redactor::apply_json`, never refusing |

- **The redactor** is the same rule as `client::spool_redactor`: `config::load_effective(home)`, `redact` forced to true,
  degraded meaning block mode, then
  `alice_core::settings::parse_project(".alice.toml", text).map(|p| p.apply_to(&global))` for `File`, and
  `ProjectSettings::fail_closed()` for `Unreadable` or a parse error.
  - Move the shared part of `spool_redactor` into `redactor_for`, and have `spool_redactor` call it for local mode, so the
    two never drift. Local `spool_redactor` keeps its current semantics, with `redact` not forced: add a `force_on: bool`
    parameter to the shared function.
- **Idempotency.** `prepare_remember` sets `idempotency_key` to `Some(new_id())` when absent.
- **Wiring in `client.rs`.** In remote mode, each named method calls its `prepare_*` on `tokio::task::spawn_blocking`
  (git and file reads), then sends.
  - `remember_via`: when `client` is a remote target, or the configuration says remote (call `remote_config::load(home)`;
    `Ok(Some(_))` means remote even when the connect failed), the spool path stores `prepare_remember(..)` instead of
    `redacted_for_spool(..)`. `ingest_or_spool` does the same.
  - Keep a process-wide `ProjectCache` in a `static` `LazyLock` for the spool path. The connected client uses its
    `RemoteState.projects`, the same static instance.
- **`CompatCall`** gains:

```rust
    /// Server mode: the project the remote client detected (spec §4.1). Wins over `cwd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_project: Option<alice_core::api::ClientProject>,
```

  and every request compat builds with `cwd: call.cwd.clone()` also sets `client_project: call.client_project.clone()`.

- [ ] **Step 1: Write the failing unit tests** (in `remote_prepare.rs`; they run `git` in temporary directories as
  `alice_core::project` tests do):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alice_core::api::*;

    fn git_repo(d: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
        let p = d.path().join(name);
        std::fs::create_dir(&p).unwrap();
        let ok = std::process::Command::new("git").arg("-C").arg(&p).args(["init", "-q"]).status().unwrap().success();
        assert!(ok);
        std::process::Command::new("git").arg("-C").arg(&p)
            .args(["remote", "add", "origin", &format!("git@github.com:me/{name}.git")]).status().unwrap();
        p
    }

    fn home(d: &tempfile::TempDir) -> AliceHome {
        let h = AliceHome::new(d.path().join("client-home"));
        h.ensure_dirs().unwrap();
        h
    }

    #[test]
    fn identity_and_rules_travel_and_cwd_does_not() {
        let d = tempfile::tempdir().unwrap();
        let repo = git_repo(&d, "app");
        std::fs::write(repo.join(".alice.toml"), "[privacy]\ncloud_denied = true\n").unwrap();
        let cache = ProjectCache::default();
        let req = prepare_remember(&home(&d), &cache, RememberRequest {
            content: "a decision".into(), cwd: Some(repo.to_string_lossy().into_owned()), ..Default::default()
        }).unwrap();
        assert!(req.cwd.is_none());
        assert!(req.idempotency_key.is_some());
        let cp = req.client_project.unwrap();
        assert_eq!((cp.key.as_str(), cp.slug.as_str()), ("github.com/me/app", "app"));
        assert_eq!(cp.rules, ClientRules::File { text: "[privacy]\ncloud_denied = true\n".into() });
    }

    #[test]
    fn secrets_are_redacted_and_block_mode_refuses_here() {
        let d = tempfile::tempdir().unwrap();
        let repo = git_repo(&d, "app");
        let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
        let cache = ProjectCache::default();
        let cwd = Some(repo.to_string_lossy().into_owned());
        let r = prepare_remember(&home(&d), &cache, RememberRequest {
            content: format!("deploy with {token}"), cwd: cwd.clone(), ..Default::default()
        }).unwrap();
        assert!(!r.content.contains(token), "{}", r.content);
        let q = prepare_recall(&home(&d), &cache, RecallRequest { query: format!("what is {token}"), cwd: cwd.clone(), ..Default::default() });
        assert!(!q.query.contains(token));
        std::fs::write(repo.join(".alice.toml"), "[privacy]\nblock_secrets = true\n").unwrap();
        let cache = ProjectCache::default();
        let e = prepare_remember(&home(&d), &cache, RememberRequest {
            content: format!("deploy with {token}"), cwd, ..Default::default()
        }).unwrap_err();
        assert!(alice_core::redact::is_refusal(&e), "{e}");
    }

    #[test]
    fn an_odd_rules_file_is_unreadable_never_none() {
        let d = tempfile::tempdir().unwrap();
        let cache = ProjectCache::default();
        let check = |repo: &std::path::Path, want: &str| {
            let (cp, _) = client_project(&ProjectCache::default(), &repo.to_string_lossy()).unwrap();
            match cp.rules {
                ClientRules::Unreadable { reason } => assert!(reason.contains(want), "{reason}"),
                other => panic!("{other:?}"),
            }
        };
        let a = git_repo(&d, "a");
        std::fs::create_dir(a.join(".alice.toml")).unwrap();
        check(&a, "directory");
        let b = git_repo(&d, "b");
        std::fs::write(b.join(".alice.toml"), vec![0xff, 0xfe, 0x00]).unwrap();
        check(&b, "UTF-8");
        let c = git_repo(&d, "c");
        std::fs::write(c.join(".alice.toml"), "#".repeat(64 * 1024 + 1)).unwrap();
        check(&c, "64 KiB");
        #[cfg(unix)]
        {
            let e = git_repo(&d, "e");
            let outside = d.path().join("outside.toml");
            std::fs::write(&outside, "[privacy]\n").unwrap();
            std::os::unix::fs::symlink(&outside, e.join(".alice.toml")).unwrap();
            check(&e, "outside");
        }
        drop(cache);
    }

    #[test]
    fn each_cwd_gets_its_own_project_from_one_cache() {
        let d = tempfile::tempdir().unwrap();
        let (x, y) = (git_repo(&d, "x"), git_repo(&d, "y"));
        let cache = ProjectCache::default();
        let one = |p: &std::path::Path| client_project(&cache, &p.to_string_lossy()).unwrap().0.slug;
        assert_eq!(one(&x), "x");
        assert_eq!(one(&y), "y");
        assert_eq!(one(&x), "x");
    }

    #[test]
    fn preparing_twice_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        let repo = git_repo(&d, "app");
        let cache = ProjectCache::default();
        let once = prepare_remember(&home(&d), &cache, RememberRequest {
            content: "note".into(), cwd: Some(repo.to_string_lossy().into_owned()), ..Default::default() }).unwrap();
        let twice = prepare_remember(&home(&d), &cache, once.clone()).unwrap();
        assert_eq!(serde_json::to_value(&once).unwrap(), serde_json::to_value(&twice).unwrap());
    }
}
```

  And integration tests in `tests/server_mode.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn a_secret_never_reaches_the_server_and_rules_follow_the_project() {
    alice_daemon::install_tls();
    let srv = Server::start(&["ann"]).await;
    let (d, home) = client_home();
    alice_daemon::remote_config::save(&home, &srv.url("ann"), &srv.tenant("ann").token).unwrap();
    let repo = d.path().join("app");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join(".alice.toml"), "[privacy]\ncloud_denied = true\n").unwrap();
    let token = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");
    let r = alice_daemon::client::remember_or_spool(&home, &alice_daemon::client::Launcher::None,
        alice_core::api::RememberRequest { content: format!("deploy with {token}"),
            cwd: Some(repo.to_string_lossy().into_owned()), ..Default::default() }).await.unwrap();
    assert_eq!(r.project, "app");
    for (_, _, _, body) in srv.seen("ann") {
        assert!(!String::from_utf8_lossy(&body).contains(token), "a secret crossed the network");
        assert!(!String::from_utf8_lossy(&body).contains(&*repo.to_string_lossy()), "a local path crossed (cwd)");
    }
    let t = srv.tenant("ann");
    let mirror = alice_daemon::client_projects::mirror_dir(&t.home, &format!("path:{}", repo.canonicalize().unwrap().display()));
    assert_eq!(std::fs::read_to_string(mirror.join(".alice.toml")).unwrap(), "[privacy]\ncloud_denied = true\n");
}
```

The plain directory `app` is keyed `path:<canonical dir>` by `detect_project`, so the mirror key is that.

- [ ] **Step 2: Run to see them fail.** Run: `cargo test -p alice-daemon --lib remote_prepare` and
  `cargo test -p alice-daemon --test server_mode a_secret_never`. Expected: compile errors.
- [ ] **Step 3: Implement** per the rules above.
  - **Reading the file:** use `std::fs::symlink_metadata` first. For a symlink, `canonicalize` it and require it to stay
    under the canonical `root`. Then `metadata().is_file()`, the size check, and `std::fs::read`, followed by
    `String::from_utf8`.
  - **`ProjectCache`:** a `parking_lot::Mutex<HashMap<String, (Option<ProjectRef>, Instant)>>` with a 60 s TTL. The rules
    file is read on every call, never cached, so an edit takes effect at once.
- [ ] **Step 4: Run the tests and the whole client suite.** Run: `cargo test -p alice-daemon --lib remote_prepare`,
  `cargo test -p alice-daemon --lib client::`, `cargo test -p alice-daemon --lib compat::`,
  `cargo test -p alice-daemon --test server_mode`. Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add crates/alice-daemon/src/remote_prepare.rs crates/alice-daemon/src/lib.rs crates/alice-daemon/src/client.rs crates/alice-daemon/src/compat.rs crates/alice-daemon/tests/server_mode.rs
git commit -m "feat(client): detect the project and redact on the client before anything leaves (remote mode)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 11: Remote replay (`remote_replay.rs`)

**Files:**
- Create: `crates/alice-daemon/src/remote_replay.rs`
- Modify: `crates/alice-daemon/src/lib.rs`
- Test: in-file unit tests; `tests/server_mode.rs`

**Interfaces:**
- Consumes: `spool::{SpoolSink, Applied, drain, pending_count}` (Task 7); `DaemonClient::send_raw` and
  `refusal_retry_later` (Tasks 6 and 9); `remote_prepare::{prepare_remember, prepare_ingest}` (Task 10).
- Produces:

```rust
/// Sends spooled entries to the remote tenant, oldest first (spec §3.5).
pub struct RemoteSink<'a> { pub client: &'a DaemonClient, pub home: &'a AliceHome }

/// Replay under `spool/.replay.lock` (taken without waiting; `None` when another process holds it
/// or nothing is pending), for at most `budget`.
pub async fn replay(home: &AliceHome, client: &DaemonClient, budget: Duration) -> Option<spool::DrainReport>;
```

**`RemoteSink::apply`:**
- prepares the entry first (`prepare_remember` or `prepare_ingest` on the blocking pool, with the process-wide cache), so a
  local-mode entry with `cwd` and no `client_project` is filed under the right project, and an entry without
  `idempotency_key` gets one. A preparation refusal (block mode on this machine) gives
  `Applied::Refused { retry_later: false, .. }`;
- posts `/api/v1/remember` or `/api/v1/ingest` with `send_raw`, then maps the answer:

  | Answer | Result |
  |---|---|
  | 2xx (202 too: the tenant spooled it on its side) | `Done` |
  | 400 with `refused` | `Refused { retry_later: refusal_retry_later(&body) }` |
  | 400 or 404 otherwise | `Rejected(body["error"])` |
  | 401 | `Paused("the server refused the token …")` |
  | a transport error, 5xx, 503 `read_only` | `Paused` |

  Note that **a prepared entry is rewritten in place** before it is sent: if the rename fails, it is sent as prepared
  anyway. The rewrite is optional; skip it.

The lock is `std::fs::File::try_lock` on `spool/.replay.lock`, created with `create(true)`. The lock file has no `.json`
extension, so `pending_count` ignores it. The budget wraps the drain in `tokio::time::timeout`, and a timeout leaves the
rest pending.

- [ ] **Step 1: Write the failing tests** (`tests/server_mode.rs`):

```rust
#[tokio::test(flavor = "multi_thread")]
async fn writes_queue_while_the_server_is_away_and_replay_once_in_order() {
    alice_daemon::install_tls();
    let srv = Server::start(&["ann"]).await;
    let (d, home) = client_home();
    alice_daemon::remote_config::save(&home, &srv.url("ann"), &srv.tenant("ann").token).unwrap();
    let repo = d.path().join("app");
    std::fs::create_dir(&repo).unwrap();
    srv.pause().await;
    for i in 0..3 {
        let r = alice_daemon::client::remember_or_spool(&home, &alice_daemon::client::Launcher::None,
            alice_core::api::RememberRequest { content: format!("queued note {i}"), cwd: Some(repo.to_string_lossy().into_owned()), ..Default::default() }).await.unwrap();
        assert_eq!(r.status, alice_core::api::RememberStatus::Spooled);
    }
    let raw = std::fs::read_dir(home.spool_dir()).unwrap().filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .map(|e| std::fs::read_to_string(e.path()).unwrap()).collect::<Vec<_>>().join("\n");
    assert!(raw.contains("client_project") && !raw.contains("\"cwd\""), "entries are prepared when spooled");
    srv.resume().await;
    let c = alice_daemon::client::DaemonClient::try_connect(&home, &alice_daemon::client::Launcher::None, std::time::Duration::ZERO).await.unwrap();
    // Two replayers at once: one gets the lock, the other skips.
    let (a, b) = tokio::join!(
        alice_daemon::remote_replay::replay(&home, &c, std::time::Duration::from_secs(5)),
        alice_daemon::remote_replay::replay(&home, &c, std::time::Duration::from_secs(5)));
    assert_eq!(a.map(|r| r.processed).unwrap_or(0) + b.map(|r| r.processed).unwrap_or(0), 3);
    assert_eq!(alice_daemon::spool::pending_count(&home.spool_dir()), 0);
    // A second replay sends nothing; the tenant holds exactly three notes, in order.
    assert!(alice_daemon::remote_replay::replay(&home, &c, std::time::Duration::from_secs(5)).await.is_none());
    let t = srv.tenant("ann");
    let conn = alice_core::store::open_reader(&t.home.db()).unwrap();
    let bodies: Vec<String> = conn.prepare("SELECT body FROM memories WHERE body LIKE 'queued note%' ORDER BY seq").unwrap()
        .query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(bodies, vec!["queued note 0", "queued note 1", "queued note 2"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_entry_spooled_by_an_older_local_client_replays_under_its_project() {
    alice_daemon::install_tls();
    let srv = Server::start(&["ann"]).await;
    let (d, home) = client_home();
    let repo = d.path().join("legacy");
    std::fs::create_dir(&repo).unwrap();
    alice_daemon::spool::append(&home.spool_dir(), &alice_daemon::spool::SpoolEntry::Remember(alice_core::api::RememberRequest {
        content: "from before remote mode".into(), cwd: Some(repo.to_string_lossy().into_owned()), ..Default::default() })).unwrap();
    alice_daemon::remote_config::save(&home, &srv.url("ann"), &srv.tenant("ann").token).unwrap();
    let c = alice_daemon::client::DaemonClient::try_connect(&home, &alice_daemon::client::Launcher::None, std::time::Duration::ZERO).await.unwrap();
    let r = alice_daemon::remote_replay::replay(&home, &c, std::time::Duration::from_secs(5)).await.unwrap();
    assert_eq!(r.processed, 1);
    let projects = c.projects().await.unwrap();
    assert!(projects.iter().any(|p| p.slug == "legacy"), "{projects:?}");
}
```

  Add a unit test in `remote_replay.rs` for the mapping, with a scripted `send_raw`. Factor the mapping into
  `fn applied_from(status: Result<(StatusCode, Value)>) -> Applied` and test it directly: 202 → `Done`; 400 with `refused`
  and `retry_later` → `Refused { retry_later: true }`; 400 plain → `Rejected`; 401 → `Paused`; 503 → `Paused`; transport
  error → `Paused`.

- [ ] **Step 2: Run to see them fail.** Run: `cargo test -p alice-daemon --test server_mode writes_queue` and
  `cargo test -p alice-daemon --lib remote_replay`. Expected: compile errors.
- [ ] **Step 3: Implement** as above.
- [ ] **Step 4: Run to see them pass.** Same commands. Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add crates/alice-daemon/src/remote_replay.rs crates/alice-daemon/src/lib.rs crates/alice-daemon/tests/server_mode.rs
git commit -m "feat(client): replay the spool to the remote tenant under a lock, in order, once

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 12: CLI and `alice mcp` in remote mode

**Files:**
- Modify: `crates/alice/src/main.rs` (`Command::Remote(cmd::RemoteCmd)`)
- Modify: `crates/alice/src/cmd.rs`:
  - `RemoteCmd`, `remote_set`, `remote_status`, `remote_unset`;
  - the refusals of spec §3.6;
  - `status` and `doctor_command` remote branches;
  - the replay after one-shot commands.
- Modify: `crates/alice-daemon/src/mcp.rs` (`run_stdio` background replay; `RemoteBackend::admin_tools` uses the cached client in remote mode)
- Test: `crates/alice/tests/cli.rs` (new tests against a real `alice daemon run --tenant` on loopback); unit tests in `mcp.rs` if practical

**Interfaces:**
- Consumes: Tasks 8–11.
- Produces the CLI surface:
  - `alice remote set <url>`: the token on stdin when it is not a TTY, else a prompt with echo off (`read_secret`);
  - `alice remote status`: exit 0 when connected, 1 otherwise; `--json` prints
    `{url, source, connected, version, capabilities, rtt_ms, pending}`;
  - `alice remote unset`.

**Behaviour:**
- **`remote set`** normalizes the URL, reads the token, then connects **without saving**: build a `RemoteConfig` in memory
  and call a new `DaemonClient::connect_with(home, &cfg)`, exposed from Task 9's `connect_remote`. It requires success,
  calls `health()`, and only then calls `remote_config::save`. It prints:
  - `Connected to <url> (Alice <version>). This client now uses the memory server.`;
  - `N queued writes will be sent to the server.` when `pending_count > 0`;
  - when `daemon.json` names a live local daemon (`DaemonClient::try_existing` with the remote config ignored; call the
    local path directly), the warning `A local daemon is still running; its memories stay on this machine ("alice daemon stop" if you no longer need it).`;
  - when the environment variables are set, the warning `ALICE_REMOTE_URL and ALICE_REMOTE_TOKEN are set and override this file.`
  - Failure saves nothing and exits 1 with the `Unavailable` message.
- **`remote status`** never prints the token. Output lines: `URL`, `Source: file|environment`,
  `Server: Alice <v> (capabilities …)` or `Server: not reachable — <why>`, `Round trip: <ms> ms`, `Queued writes: N`. Use
  `DaemonClient::server_ping()` for a fresh proof and the round trip.
- **Refusals in remote mode.** Check `remote_config::load(&home)`: `Ok(Some(_))` means remote, and an error means remote
  misconfigured, so refuse with the error. Then refuse, with the exact texts of spec §3.6:

  | Command | Refusal message |
  |---|---|
  | `daemon start`, `daemon stop`, `daemon status`, `daemon logs` | `this client uses the memory server <url>; "alice remote unset" to use a local daemon` |
  | `token rotate` | `the token of a server tenant is rotated by its admin ("alice-server rotate <name>"); then run "alice remote set"` |
  | `restore` | `restore replaces local files; a server tenant's restore is a task for the server's admin` |
  | `service install`, `service uninstall` | `this client uses the memory server <url>; a login service would start a local daemon` |
  | `config edit` | `this client uses the memory server <url>; change settings with "alice config set"` |

  `service status` prints `remote mode: <url>` and the usual status. `daemon run` is never refused.
- **`status`:** in remote mode, prints the `remote status` lines plus the server's health summary (the existing
  `health_notes`).
- **`doctor`:** in remote mode, runs these checks, then appends the server's `/api/v1/doctor` checks:
  - `remote.json` mode (0600);
  - the URL scheme;
  - the ping proof;
  - capabilities;
  - version skew (warn when the client's and the server's versions differ);
  - queued writes.

  It skips the local daemon checks.
- **One-shot replay.** In `client_command`, after the command's own work succeeds, when the client in use is remote and
  `pending_count > 0`, call `remote_replay::replay(&home, &client, Duration::from_secs(2))` and ignore its result. Restructure
  so that `client(&home)` is called once per command and kept, or call `DaemonClient::try_existing(&home)` again; remote
  mode makes that cheap. Never replay in local mode: the daemon drains its own spool.
- **`alice mcp`:** in `run_stdio`, when remote mode is configured, spawn a task that every 30 s, while
  `pending_count > 0`, gets a client through the backend's `try_client(READ_WAIT)` and runs `replay(…, 10 s)`. It stops
  when the server future ends.
- **`RemoteBackend::admin_tools`:** in remote mode, uses `self.try_client(Duration::ZERO)` (the cached, verified client)
  instead of `DaemonClient::try_existing`, with the remote flag timeout of 3 s.

- [ ] **Step 1: Write the failing CLI tests** in `crates/alice/tests/cli.rs`. They use the existing `Env` helper, start a
  tenant daemon as a separate process with `alice daemon run --tenant`, `ALICE_HOME=<server home>`,
  `ALICE_EMBEDDER=fake` and `ALICE_SERVER_PORT=<a free port>`, then read its token from `<server home>/token`:

```rust
#[test]
fn remote_mode_round_trip_through_a_tenant_daemon() {
    let env = Env::new();
    let server = TenantDaemon::start(); // helper: spawns `alice daemon run --tenant` on a free port, waits for its ping
    let url = format!("http://127.0.0.1:{}", server.port);
    let mut set = env.cmd(&["remote", "set", &url]);
    set.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = set.spawn().unwrap();
    child.stdin.take().unwrap().write_all(server.token.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("Connected to"));
    let r = env.cmd(&["remember", "the staging db is on port 5432"]).output().unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let r = env.cmd(&["recall", "staging db port"]).output().unwrap();
    assert!(String::from_utf8_lossy(&r.stdout).contains("5432"));
    let s = env.cmd(&["remote", "status"]).output().unwrap();
    assert!(s.status.success());
    let all = format!("{}{}", String::from_utf8_lossy(&s.stdout), String::from_utf8_lossy(&s.stderr));
    assert!(!all.contains(&server.token), "the token is never printed");
    for args in [&["daemon", "start"][..], &["token", "rotate"], &["restore", "--yes"], &["config", "edit"], &["service", "install"]] {
        let o = env.cmd(args).output().unwrap();
        assert!(!o.status.success(), "{args:?} must be refused in remote mode");
        assert!(String::from_utf8_lossy(&o.stderr).contains("memory server") || String::from_utf8_lossy(&o.stderr).contains("admin"),
            "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
    }
    assert!(!env.home.join("daemon.json").exists(), "no local daemon was started");
    let u = env.cmd(&["remote", "unset"]).output().unwrap();
    assert!(u.status.success());
}

#[test]
fn a_wrong_token_saves_nothing_and_the_token_is_absent_from_debug_logs() {
    let env = Env::new();
    let server = TenantDaemon::start();
    let url = format!("http://127.0.0.1:{}", server.port);
    let mut set = env.cmd(&["remote", "set", &url]);
    set.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).env("ALICE_LOG", "debug");
    let mut child = set.spawn().unwrap();
    let wrong = "f".repeat(64);
    child.stdin.take().unwrap().write_all(wrong.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(!env.home.join("remote.json").exists());
    let all = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!all.contains(&wrong) && !all.contains(&server.token));
}
```

  `TenantDaemon` is a new helper in `cli.rs`. It holds a `TempDir` server home and the child process, picks a free port
  by binding `127.0.0.1:0` and dropping the listener, waits up to 20 s for `GET /api/v1/ping` with
  `Host: 127.0.0.1:<port>` (use `std::net::TcpStream` and a raw HTTP/1.1 request, since the CLI test crate has no HTTP
  client), and on `Drop` kills the process and waits for it.

- [ ] **Step 2: Run to see them fail.** Run: `cargo test -p alice-memory --test cli remote_mode -- --test-threads 1`.
  Expected: FAIL (unknown subcommand `remote`).
- [ ] **Step 3: Implement** as described.
- [ ] **Step 4: Run the new tests, then the whole CLI suite**, since every command passes through the new branches.
  Run: `cargo test -p alice-memory --test cli -- --test-threads 2`, then `cargo test -p alice-daemon --lib mcp::`.
  Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add crates/alice/src/main.rs crates/alice/src/cmd.rs crates/alice-daemon/src/mcp.rs crates/alice-daemon/src/client.rs crates/alice/tests/cli.rs
git commit -m "feat(cli): alice remote set/status/unset, remote-mode refusals, status and doctor, replay after commands and in alice mcp

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---
### Task 13: The bundle format — `Backend::Bundle`, `BundleWriter`, `BundleReader`

**Files:**
- Create: `crates/alice-core/src/import/bundle.rs`
- Modify:
  - `crates/alice-core/src/import/mod.rs`: `pub mod bundle;` and `ImportRequest::bundle`;
  - `crates/alice-core/src/import/palace.rs`: `Backend::Bundle`, plus arms in `open_snapshot` and `count_closets`;
  - every other exhaustive `match` on `Backend` (`crates/alice-daemon/src/importer.rs`, `crates/alice/src/cmd.rs`,
    `crates/alice-core/src/import/chroma.rs`; `cargo check` finds them), with a `"bundle"` name wherever a backend is
    printed.
- Test: in-file tests (the `fixture` module is available under `cfg(test)`)

**Interfaces:**
- Produces:

```rust
pub const BUNDLE_FILE: &str = "bundle.sqlite3";
pub const FORMAT: &str = "alice-import-bundle";
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BundleMeta {
    pub source_backend: String,   // "sqlite_exact" | "chroma"
    pub collection: String,
    pub drawers: u64,
    pub closets: u64,
    pub held_back: u64,
    pub notes: Vec<String>,
    pub created_at: String,
}

pub struct BundleWriter { /* rusqlite::Connection */ }
impl BundleWriter {
    /// A new bundle at `path` (must not exist), private (0600), with the schema of spec §4.5.
    pub fn create(path: &Path) -> Result<Self>;
    /// One drawer, already redacted by the caller.
    pub fn drawer(&mut self, d: &Drawer) -> Result<()>;
    /// One entity row of MemPalace's graph schema.
    pub fn entity(&mut self, id: &str, name: &str) -> Result<()>;
    /// One triple (subject and object are entity ids).
    pub fn triple(&mut self, t: &BundleTriple) -> Result<()>;
    pub fn tunnels(&mut self, json: &str) -> Result<()>;
    /// Writes the metadata and commits; the file is complete only after this.
    pub fn finish(self, meta: &BundleMeta) -> Result<()>;
}

pub struct BundleTriple<'a> {
    pub id: &'a str, pub subject: &'a str, pub predicate: &'a str, pub object: &'a str,
    pub valid_from: Option<&'a str>, pub valid_to: Option<&'a str>,
    pub source_drawer_id: Option<&'a str>, pub extracted_at: Option<&'a str>,
}

/// Opens a bundle read-only (`trusted_schema = OFF`), checks format and version and runs `quick_check`.
pub fn check(path: &Path) -> Result<BundleMeta>;
/// The snapshot the import job reads: `dir` holds `bundle.sqlite3`; the tunnels (if any) are written to `dir/tunnels.json`.
pub fn snapshot_of(dir: &Path) -> Result<Snapshot>;

pub struct BundleReader { /* Connection, PalaceInfo */ }
impl PalaceReader for BundleReader { /* page by id with `since` on filed_at, get, wing_counts */ }
```

`ImportRequest` gains:

```rust
    /// Server mode: an uploaded bundle (`PUT /api/v1/import/bundle`) to import instead of a palace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
```

**Schema** (spec §4.5), in one `execute_batch`:

```sql
CREATE TABLE bundle_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE bundle_drawers (id TEXT PRIMARY KEY, text TEXT NOT NULL, wing TEXT NOT NULL, filed_at TEXT, meta TEXT NOT NULL);
CREATE INDEX bundle_drawers_wing ON bundle_drawers(wing);
CREATE TABLE entities (id TEXT PRIMARY KEY, name TEXT NOT NULL, type TEXT DEFAULT 'unknown',
    properties TEXT DEFAULT '{}', created_at TEXT DEFAULT CURRENT_TIMESTAMP);
CREATE TABLE triples (id TEXT PRIMARY KEY, subject TEXT NOT NULL, predicate TEXT NOT NULL, object TEXT NOT NULL,
    valid_from TEXT, valid_to TEXT, confidence REAL DEFAULT 1.0, source_closet TEXT, source_file TEXT,
    source_drawer_id TEXT, adapter_name TEXT, extracted_at TEXT);
CREATE TABLE bundle_tunnels (json TEXT NOT NULL);
```

**Details:**
- **`meta`** is the `DrawerMeta` serialised through a private serde struct with the same fields. `DrawerMeta` itself has no
  serde derive; do not add one to M3's struct.
- **`page`** mirrors `SqliteExactReader::page`:
  `WHERE id > ?1 AND (?2 IS NULL OR filed_at IS NULL OR filed_at >= ?2) ORDER BY id LIMIT ?3`.
- **`wing_counts`** is `SELECT wing, count(*) … GROUP BY wing`, sorted as the other readers sort (by count, then name).
- **`info()`** is `PalaceInfo { backend: Backend::Bundle, path: <file>, drawers: <count> }`.
- **`snapshot_of(dir)`** returns
  `Snapshot { dir, backend: Bundle, source: dir/bundle.sqlite3, kg: Some(dir/bundle.sqlite3) when triples exist else None, tunnels: Some(dir/tunnels.json) when bundle_tunnels has a row (written there now, 0600) else None, notes: meta.notes }`.
- **`count_closets`** for `Bundle` reads `bundle_meta.closets`.
- **`open_snapshot`** for `Bundle` gives `BundleReader::open(&s.dir.join(BUNDLE_FILE))`.

- [ ] **Step 1: Write the failing tests** (in `bundle.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::fixture::synthetic_palace;
    use crate::import::palace::{open_snapshot, snapshot};

    /// Copies every drawer of a palace into a bundle unchanged (redaction is the caller's job, Task 14).
    fn plain_bundle(palace: &Path, dir: &Path) -> std::path::PathBuf {
        let snap_dir = dir.join("snap");
        let snap = snapshot(palace, &snap_dir, None).unwrap();
        let reader = open_snapshot(&snap, None).unwrap();
        let out = dir.join("b").join(BUNDLE_FILE);
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        let mut w = BundleWriter::create(&out).unwrap();
        let mut after: Option<String> = None;
        let mut n = 0;
        loop {
            let page = reader.page(after.as_deref(), 500, None).unwrap();
            if page.is_empty() { break; }
            for d in &page { w.drawer(d).unwrap(); n += 1; }
            after = page.last().map(|d| d.id.clone());
        }
        w.finish(&BundleMeta { source_backend: "sqlite_exact".into(), collection: "mempalace_drawers".into(),
            drawers: n, created_at: "2026-10-15T00:00:00Z".into(), ..Default::default() }).unwrap();
        out
    }

    #[test]
    fn a_bundle_reads_back_like_the_palace() {
        let d = tempfile::tempdir().unwrap();
        let p = synthetic_palace(&d.path().join("palace"), 450);
        let file = plain_bundle(&p.dir, d.path());
        let meta = check(&file).unwrap();
        assert_eq!(meta.drawers, 450);
        let snap = snapshot_of(file.parent().unwrap()).unwrap();
        assert_eq!(snap.backend, Backend::Bundle);
        let from_bundle = open_snapshot(&snap, None).unwrap();
        let src = open_snapshot(&snapshot(&p.dir, &d.path().join("snap2"), None).unwrap(), None).unwrap();
        assert_eq!(from_bundle.wing_counts().unwrap(), src.wing_counts().unwrap());
        let (a, b) = (from_bundle.page(None, 1000, None).unwrap(), src.page(None, 1000, None).unwrap());
        assert_eq!(a, b, "drawers, text and metadata survive the round trip");
        assert_eq!(from_bundle.page(None, 1000, Some("2026-04-16")).unwrap().len(),
                   src.page(None, 1000, Some("2026-04-16")).unwrap().len());
        let ids: Vec<String> = b.iter().take(3).map(|d| d.id.clone()).collect();
        assert_eq!(from_bundle.get(&ids).unwrap(), src.get(&ids).unwrap());
    }

    #[test]
    fn check_refuses_what_is_not_a_bundle() {
        let d = tempfile::tempdir().unwrap();
        let not_sqlite = d.path().join("x");
        std::fs::write(&not_sqlite, b"hello").unwrap();
        assert!(check(&not_sqlite).is_err());
        let other = d.path().join("y.sqlite3");
        rusqlite::Connection::open(&other).unwrap().execute_batch("CREATE TABLE t (a);").unwrap();
        assert!(check(&other).is_err());
        let p = synthetic_palace(&d.path().join("palace"), 10);
        let file = plain_bundle(&p.dir, d.path());
        rusqlite::Connection::open(&file).unwrap().execute("UPDATE bundle_meta SET value = '2' WHERE key = 'version'", []).unwrap();
        assert!(check(&file).unwrap_err().to_string().contains("version"));
    }

    #[test]
    fn the_graph_and_tunnels_in_a_bundle_read_with_the_existing_readers() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("b");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(BUNDLE_FILE);
        let mut w = BundleWriter::create(&file).unwrap();
        w.entity("e1", "alice").unwrap();
        w.entity("e2", "postgres").unwrap();
        w.triple(&BundleTriple { id: "t1", subject: "e1", predicate: "uses", object: "e2", valid_from: Some("2026-01-01"),
            valid_to: None, source_drawer_id: None, extracted_at: None }).unwrap();
        w.tunnels(r#"[{"source":{"wing":"a","room":"r","drawer_id":"d1"},"target":{"wing":"b","room":"r","drawer_id":"d2"},"kind":"explicit"}]"#).unwrap();
        w.finish(&BundleMeta { drawers: 0, ..Default::default() }).unwrap();
        let snap = snapshot_of(&dir).unwrap();
        let kg = crate::import::kg::read_kg(snap.kg.as_ref().unwrap()).unwrap();
        assert_eq!(kg.triples.len(), 1);
        assert_eq!((kg.triples[0].subject.as_str(), kg.triples[0].object.as_str()), ("alice", "postgres"));
        let t = crate::import::kg::read_tunnels(snap.tunnels.as_ref().unwrap());
        assert_eq!(t.explicit.len() as u64 + t.dropped_topic + t.dropped_malformed, 1);
    }
}
```

The tunnels JSON shape above must be one `read_tunnels` accepts. Look at `fixture::build_tunnels` callers in M3's tests
for a valid explicit tunnel, and use that exact shape.

- [ ] **Step 2: Run to see them fail.** Run: `cargo test -p alice-core import::bundle`. Expected: compile errors.
- [ ] **Step 3: Implement** per the interfaces.
  - `BundleWriter::create` opens the file with `create_new` and mode 0600, then `Connection::open`,
    `PRAGMA journal_mode = OFF` (a bundle is written once) and `BEGIN`. `finish` inserts the meta (format and version
    included) and commits.
  - `check` opens with `SQLITE_OPEN_READ_ONLY`, runs `PRAGMA trusted_schema = OFF` and `PRAGMA quick_check` (expects
    `ok`), then checks the meta: a wrong format gives "not an Alice import bundle", and a wrong version gives
    "bundle version {v} is not supported by this server (it reads version 1); upgrade the server or the client so they
    match".
  - Then add the `Backend::Bundle` arms and run `cargo check --workspace --all-targets` to find every other match.
- [ ] **Step 4: Run to see them pass.** Run: `cargo test -p alice-core import::`, then
  `cargo check --workspace --all-targets`. Expected: PASS, and no compile errors.
- [ ] **Step 5: Commit**

```bash
git add crates/alice-core/src/import/bundle.rs crates/alice-core/src/import/mod.rs crates/alice-core/src/import/palace.rs crates/alice-core/src/import/chroma.rs crates/alice-daemon/src/importer.rs crates/alice/src/cmd.rs
git commit -m "feat(import): the import bundle — Backend::Bundle, writer, checked reader (server mode, R20/R21)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 14: The redacting bundle builder (client side)

**Files:**
- Create: `crates/alice-daemon/src/import_bundle.rs`
- Modify: `crates/alice-daemon/src/lib.rs`
- Test: in-file tests

**Interfaces:**
- Consumes: `BundleWriter`, `BundleMeta` and `BundleTriple` (Task 13); `importer::RedactFilter` (M3, public); `snapshot`,
  `open_snapshot`, `count_closets`, `read_kg` (M3); `remote_prepare::redactor_for` (Task 10).
- Produces:

```rust
pub struct Built {
    pub path: PathBuf,
    pub bytes: u64,
    pub drawers: u64,
    /// Detector name -> drawers and triples held back on this machine (block mode).
    pub blocked: BTreeMap<String, u64>,
    pub held_back: u64,
    pub redacted_fields: u64,
}

/// Snapshot `palace` into `work` (a private folder), write `work/bundle/bundle.sqlite3` with every text redacted
/// (spec §3.8 step 2), delete the snapshot copy, and return what was written. Blocking.
pub fn build(palace: &Path, mempalace_home: Option<&Path>, collection: Option<&str>, redactor: Arc<Redactor>, work: &Path) -> Result<Built>;
```

**Rules** (spec §3.8 step 2):

| Item | Rule |
|---|---|
| Filter | `let f = RedactFilter::new(redactor)` (the redactor from `remote_prepare::redactor_for(home, None)`, global rules forced on) |
| Drawer text | `f.filter("drawer", &d.text)`. `Ok(t)` stores `t`. `Err(detectors)` holds the drawer back: add it to `blocked` under each detector, `held_back += 1`, and record its `legacy_key` in a `HashSet` of held-back drawer ids |
| Drawer id | `f.legacy_key(&d.id)` |
| `DrawerMeta` strings | wing, room, hall, source file, added by, ingest mode, agent, topic, entry type, parent drawer id (the last through `legacy_key`): `f.quiet` |
| Dates | `filed_at`, `authored_at` and `date` are kept as they are, but passed through `quiet` too, since they are text |
| Graph | read with `read_kg(kg)`. For each triple, run `f.filter("fact", …)` on subject, predicate and object; any refusal holds the triple back (counted in `blocked`). Kept triples are written with entity ids `"e_" + sha256_hex(name)[..24]` over the **redacted** name (one entity row per distinct name), the id `f.quiet(&t.id)` (a triple id that changed through redaction also gets `"~" + sha256_hex(original)[..16]`, as `legacy_key` does for drawers), `source_drawer_id` through `legacy_key`, and the dates as they are |
| Isolated entities | `kg.isolated_entities` placeholder rows (`id = "isolated_<i>"`, `name = "isolated entity <i>"`), so the server's report counts them the same way |
| Tunnels | read the raw file (at most 16 MB, as M3 caps it). Parse the JSON and walk it: a string under a key ending in `drawer_id` goes through `legacy_key`; every other string through `quiet`. Serialize it back into `bundle_tunnels`. A file that does not parse is copied as the empty array `[]`, plus a note |
| Meta | `source_backend` from the snapshot's backend name, `collection` (the default when `None`), `drawers` written, `closets` from `count_closets(&snap)`, `held_back`, `notes` from the snapshot plus the builder's own, `created_at` now |

The snapshot folder is deleted at the end, however the build ends (a drop guard). On an error, the half-written bundle is
deleted.

- [ ] **Step 1: Write the failing tests:**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alice_core::import::fixture::{build_kg, build_tunnels, synthetic_palace, TripleSpec};
    use alice_core::settings::{PrivacySettings, RedactMode};

    fn redactor(block: bool) -> Arc<Redactor> {
        let mut p = PrivacySettings::default();
        p.redact = true;
        if block { p.redact_mode = RedactMode::Block; }
        Arc::new(Redactor::new(&p).unwrap())
    }

    const SECRET: &str = concat!("gh", "p_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789");

    /// The synthetic palace plus one drawer and one fact holding a secret.
    fn palace(d: &tempfile::TempDir) -> std::path::PathBuf {
        let p = synthetic_palace(&d.path().join("palace"), 300);
        let conn = rusqlite::Connection::open(p.dir.join("sqlite_exact.sqlite3")).unwrap();
        // Add a drawer through the same insert the fixture uses (see fixture::build_sqlite_exact for the columns).
        crate::import_bundle::tests_support::add_drawer(&conn, "syn_secret_0000001", &format!("the deploy key is {SECRET}"), "pg-ai");
        build_kg(&p.dir, &[("e1", "alice"), ("e2", SECRET)], &[TripleSpec::new("t1", "e1", "uses", "e2")], false);
        build_tunnels(&p.dir, "[]");
        p.dir
    }

    #[test]
    fn no_secret_reaches_the_bundle_and_block_mode_holds_back() {
        for block in [false, true] {
            let d = tempfile::tempdir().unwrap();
            let pal = palace(&d);
            let built = build(&pal, None, None, redactor(block), &d.path().join("work")).unwrap();
            let bytes = std::fs::read(&built.path).unwrap();
            assert!(!bytes.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()), "block={block}: the secret is in the bundle");
            if block {
                assert_eq!(built.held_back, 2, "the drawer and the triple");
                assert!(!built.blocked.is_empty());
            } else {
                assert_eq!(built.held_back, 0);
                assert!(built.redacted_fields >= 2);
            }
            assert!(!d.path().join("work").join("snapshot").exists(), "the snapshot copy is gone");
        }
    }
}
```

  Add a `#[cfg(test)] pub(crate) mod tests_support` with `add_drawer(conn, id, text, wing)`, inserting a row the way
  `fixture::build_sqlite_exact` does: read that function and copy its insert for one drawer. If `TripleSpec` has no `new`,
  build it with its fields.

- [ ] **Step 2: Run to see them fail.** Run: `cargo test -p alice-daemon --lib import_bundle`. Expected: compile errors.
- [ ] **Step 3: Implement** per the rules.
- [ ] **Step 4: Run to see them pass.** Run: `cargo test -p alice-daemon --lib import_bundle`. Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add crates/alice-daemon/src/import_bundle.rs crates/alice-daemon/src/lib.rs
git commit -m "feat(import): build a redacted import bundle on the client; block mode holds secrets back

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 15: Server — the upload endpoint and the bundle entry point of the import job

**Files:**
- Modify: `crates/alice-daemon/src/http.rs` (route `PUT /api/v1/import/bundle` with its own body rule)
- Modify: `crates/alice-daemon/src/service.rs`:
  - `receive_bundle`;
  - `start_import` accepts `bundle`, and its tenant rule from Task 6 becomes "refuse unless `bundle`";
  - `ServiceOptions::free_space` for tests.
- Modify: `crates/alice-daemon/src/importer.rs`:
  - `preflight_bundle(req, home) -> Result<PathBuf>`, which checks the id and the file and returns the job folder;
  - in `run`, `let snap = match req.bundle { Some(_) => bundle::snapshot_of(&dir)?, None => snapshot(palace, &dir, …)? }`;
  - the job id equals the bundle id.
- Test: `tests/server_mode.rs` and unit tests in `service.rs`

**Interfaces:**
- Consumes: `bundle::{check, snapshot_of}` (Task 13); the emulator (Task 9).
- Produces:
  - `Service::receive_bundle(&self, declared: u64, body: impl Stream<Item = Result<Bytes, …>>) -> Result<BundleReceipt>`;
  - `BundleReceipt { bundle: String, bytes: u64 }` (`Serialize`);
  - `ServiceOptions::free_space: Option<Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>>` (tests inject; `None` uses
    `backup::free_space`);
  - constants `BUNDLE_MAX_BYTES = 4 << 30`, `BUNDLE_MIN_FREE_AFTER = 2_000_000_000`, `BUNDLE_SWEEP_AFTER = 1 h`.

**Behaviour** (spec §4.5): refusals in this order. Every refusal is `invalid` and says what to do.
1. An import is running → "an import is running; wait for it (`alice import status`)".
2. Another upload is in progress → "another upload is in progress". Guard it with an `AtomicBool` in the service.
3. The length is missing → "send Content-Length". The handler passes `None`.
4. The length is 0 or above `BUNDLE_MAX_BYTES` → "the bundle is {n} bytes; the server takes at most 4 GiB".
5. `free_space(home) - n < BUNDLE_MIN_FREE_AFTER` → "the server has {free} free; this {n}-byte bundle would leave less than 2 GB; ask the server's admin".

Then the server sweeps stale uploads (`import-tmp/*` folders whose `bundle.sqlite3` or `.part` is older than 1 h, while no
job runs), streams to `.part` counting bytes (more than declared, or fewer when the stream ends, means delete and refuse),
syncs, renames, runs `bundle::check`, and answers `201`. The handler is `async fn upload_bundle(State, HeaderMap, Body)`,
with `.route_layer(DefaultBodyLimit::disable())` on that one route.

`start_import` with `bundle: Some(id)`:
- `id` must be 26 characters of `[0-9A-Z]`;
- `import-tmp/<id>/bundle.sqlite3` must exist;
- `palace` must be `None`;
- the job id is `id`, so the job's folder is the upload's folder, and M3's `SnapshotDir` deletes it when the job ends.

In tenant mode, `bundle: None` is refused with Task 6's message.

- [ ] **Step 1: Write the failing tests** (in `tests/server_mode.rs`):

```rust
async fn upload(srv: &Server, name: &str, body: Vec<u8>, declared: Option<u64>) -> reqwest::Response {
    let t = srv.tenant(name);
    let mut rb = reqwest::Client::new().put(format!("{}/api/v1/import/bundle", srv.url(name))).bearer_auth(&t.token);
    if let Some(n) = declared { rb = rb.header("content-length", n); }
    rb.body(body).send().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn uploads_are_checked_before_and_after_and_leave_nothing_behind() {
    alice_daemon::install_tls();
    let srv = Server::start(&["ann"]).await;
    let tmp = srv.tenant("ann").home.import_tmp();
    let r = upload(&srv, "ann", b"not a bundle".to_vec(), Some(12)).await;
    assert_eq!(r.status(), 400);
    assert!(std::fs::read_dir(&tmp).map(|d| d.count()).unwrap_or(0) == 0, "a refused upload leaves no folder");
    srv.set_free_space("ann", Some(1_000_000_000));
    let r = upload(&srv, "ann", vec![0u8; 1024], Some(1024)).await;
    assert_eq!(r.status(), 400);
    assert!(r.text().await.unwrap().contains("2 GB"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tenant_imports_only_its_uploaded_bundle() {
    alice_daemon::install_tls();
    let srv = Server::start(&["ann"]).await;
    let t = srv.tenant("ann");
    for req in [serde_json::json!({"palace": "/srv/alice/bob"}), serde_json::json!({}), serde_json::json!({"bundle": "01JUNKJUNKJUNKJUNKJUNKJUNK"}), serde_json::json!({"bundle": "../../etc"})] {
        let r = reqwest::Client::new().post(format!("{}/api/v1/import/mempalace", srv.url("ann"))).bearer_auth(&t.token).json(&req).send().await.unwrap();
        assert_eq!(r.status(), 400, "{req}");
    }
}
```

  `Server::set_free_space(name, Option<u64>)` makes the emulator's tenant `Service` use an injected free-space function.
  Start the tenants with `ServiceOptions { free_space: Some(<a closure reading a shared AtomicU64, u64::MAX by default>), … }`.

  The full import through the bundle is tested in Task 16.

- [ ] **Step 2: Run to see them fail.** Run: `cargo test -p alice-daemon --test server_mode uploads_are` and
  `cargo test -p alice-daemon --test server_mode a_tenant_imports`. Expected: FAIL.
- [ ] **Step 3: Implement** per the behaviour. Keep M3's `run` and `start_import` changes to the few lines described
  (additive).
- [ ] **Step 4: Run to see them pass, then M3's import suites**, to be sure nothing of theirs moved.

Run: `cargo test -p alice-daemon --test server_mode`, `cargo test -p alice-daemon --test import_flow`, `cargo test -p alice-daemon --lib importer::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/alice-daemon/src/http.rs crates/alice-daemon/src/service.rs crates/alice-daemon/src/importer.rs crates/alice-daemon/tests/server_mode.rs crates/alice-daemon/tests/support/caddy.rs
git commit -m "feat(import): upload a bundle (size and disk guards) and import it with the existing job (R20, R24)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 16: `alice import mempalace` in remote mode

**Files:**
- Modify: `crates/alice-daemon/src/client.rs`:
  - `DaemonClient::upload_bundle(&self, path: &Path, progress: &dyn Fn(u64, u64)) -> Result<String>`, which streams the
    file with `reqwest::Body::wrap_stream`, sets `content-length`, has no overall timeout but a 5 s connect, reports
    progress, and returns the bundle id;
  - the `start_import` timeout follows `t()`.
- Modify: `crates/alice/src/cmd.rs` (`import_mempalace`: the remote branch)
- Test: `tests/server_mode.rs` (the end-to-end equality test) and `crates/alice/tests/cli.rs` (one CLI run)

**Behaviour** (spec §3.8 steps 1–5). When remote mode is configured, `import_mempalace`:
1. resolves the palace as today;
2. builds the bundle with `import_bundle::build(&palace, mempalace_home, collection, redactor_for(&home, None) as Arc, &home.import_tmp().join(new_id()))`
   on the blocking pool, printing `Reading and redacting the palace…`, then
   `Bundle: N drawers, M MB; held back on this machine: H (block mode)`;
3. uploads with progress (`Uploading: x of y MB`);
4. calls `start_import(&ImportRequest { bundle: Some(id), palace: None, dry_run, since, map_wing, collection, allow_remote_embedding })`;
5. polls exactly as today;
6. before printing, merges the client's `blocked` into `report.blocked_by_redaction` and adds `redacted_fields`;
7. prints the report, plus `Held back on this machine (not sent): H` when H > 0;
8. deletes the local work folder, also on error and on Ctrl-C (a drop guard).

Local mode is unchanged.

- [ ] **Step 1: Write the failing end-to-end test** (in `tests/server_mode.rs`):

```rust
#[tokio::test(flavor = "multi_thread")]
async fn a_bundle_import_matches_a_local_import_and_a_rerun_adds_nothing() {
    use alice_core::import::{ImportRequest, ImportState};
    alice_daemon::install_tls();
    let d = tempfile::tempdir().unwrap();
    let pal = alice_core::import::fixture::synthetic_palace(&d.path().join("palace"), 600);
    // Local import, in-process.
    let local_home = AliceHome::new(d.path().join("local"));
    let local = alice_daemon::service::Service::start(local_home.clone(),
        &|| Ok(std::sync::Arc::new(alice_core::embed::FakeEmbedder::new(384)) as std::sync::Arc<dyn alice_core::embed::Embedder>),
        alice_daemon::service::ServiceOptions::default()).unwrap();
    local.start_import(ImportRequest { palace: Some(pal.dir.to_string_lossy().into_owned()), ..Default::default() }).unwrap();
    let local_report = wait_import(|| local.import_status()).await;
    // Remote: bundle, upload, import.
    let srv = Server::start(&["ann"]).await;
    let (_c, home) = client_home();
    alice_daemon::remote_config::save(&home, &srv.url("ann"), &srv.tenant("ann").token).unwrap();
    let c = alice_daemon::client::DaemonClient::try_connect(&home, &alice_daemon::client::Launcher::None, std::time::Duration::ZERO).await.unwrap();
    let built = alice_daemon::import_bundle::build(&pal.dir, None, None,
        std::sync::Arc::new(alice_daemon::remote_prepare::redactor_for(&home, None)), &home.import_tmp().join("w")).unwrap();
    let id = c.upload_bundle(&built.path, &|_, _| {}).await.unwrap();
    c.start_import(&ImportRequest { bundle: Some(id), ..Default::default() }).await.unwrap();
    let remote_report = wait_import_remote(&c).await;
    for (what, a, b) in [
        ("tiers", serde_json::to_value(&local_report.tiers).unwrap(), serde_json::to_value(&remote_report.tiers).unwrap()),
        ("projects", serde_json::to_value(&local_report.projects).unwrap(), serde_json::to_value(&remote_report.projects).unwrap()),
        ("duplicates", serde_json::to_value(&local_report.duplicates).unwrap(), serde_json::to_value(&remote_report.duplicates).unwrap()),
        ("facts", serde_json::to_value(&local_report.facts).unwrap(), serde_json::to_value(&remote_report.facts).unwrap()),
        ("skipped", serde_json::to_value(&local_report.skipped).unwrap(), serde_json::to_value(&remote_report.skipped).unwrap()),
    ] {
        assert_eq!(a, b, "{what}");
    }
    assert_eq!(local_report.drawers_seen, remote_report.drawers_seen);
    assert_eq!(local_report.drawers_accounted, remote_report.drawers_accounted);
    assert!(std::fs::read_dir(srv.tenant("ann").home.import_tmp()).map(|d| d.count()).unwrap_or(0) == 0, "the bundle is gone");
    // A rerun imports nothing new.
    let built = alice_daemon::import_bundle::build(&pal.dir, None, None,
        std::sync::Arc::new(alice_daemon::remote_prepare::redactor_for(&home, None)), &home.import_tmp().join("w2")).unwrap();
    let id = c.upload_bundle(&built.path, &|_, _| {}).await.unwrap();
    c.start_import(&ImportRequest { bundle: Some(id), ..Default::default() }).await.unwrap();
    let again = wait_import_remote(&c).await;
    assert_eq!(again.already_imported, remote_report.drawers_seen - remote_report.skipped.values().sum::<u64>());
    local.shutdown();
}
```

  `wait_import` and `wait_import_remote` poll the status every 100 ms (up to 120 s) until it is not `Running`, assert
  `Done`, and return the report. The rerun assertion must follow how M3 counts a rerun: read
  `crates/alice-daemon/tests/import_flow.rs` for its "second run imports nothing" test, and assert the same fields the
  same way.

  And a CLI test in `cli.rs` (`TenantDaemon` from Task 12): run `alice import mempalace --palace <synthetic palace>`
  against it in remote mode. Build the palace with a tiny helper binary? No: the CLI tests can build it by calling the
  fixture through a test-only path. If `alice-core`'s `fixtures` feature is not available to `alice-memory`'s tests, add
  `alice-core = { path = "../alice-core", features = ["fixtures"] }` under `[dev-dependencies]` of `crates/alice`. Assert
  exit 0, a report on stdout, and that the client's `import-tmp` is empty afterwards.

- [ ] **Step 2: Run to see them fail.** Run: `cargo test -p alice-daemon --test server_mode a_bundle_import` and
  `cargo test -p alice-memory --test cli import_remote -- --test-threads 1`. Expected: FAIL.
- [ ] **Step 3: Implement** `upload_bundle` and the CLI branch.
- [ ] **Step 4: Run to see them pass.** Same commands. Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add crates/alice-daemon/src/client.rs crates/alice/src/cmd.rs crates/alice/Cargo.toml crates/alice-daemon/tests/server_mode.rs crates/alice/tests/cli.rs
git commit -m "feat(import): alice import mempalace in remote mode — redacted bundle, upload, server import, merged report

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 17: The admin script `deploy/server/alice-server` and its tests

**Files:**
- Create: `deploy/server/alice-server` (mode 0755)
- Create: `deploy/server/tests/run.sh` (mode 0755)
- Test: `bash deploy/server/tests/run.sh`; `shellcheck deploy/server/alice-server deploy/server/tests/run.sh`. If
  shellcheck is missing on this Mac, it is installed only with the controller's say-so; otherwise record it as skipped and
  run it on the server in Task 20.

**Interfaces:** the commands and outputs of spec §2.5, with the R15, R18 and R23 values; no fail2ban.

- [ ] **Step 1: Write the test runner first** (`deploy/server/tests/run.sh`). It:
  - builds a temporary root `R` and a stub directory `S` put first on `PATH`;
  - exports `ALICE_SERVER_ROOT=$R`;
  - writes `$R/etc/alice/server.env` with `ALICE_PUBLIC_HOST=203.0.113.7`;
  - creates `$R/usr/local/bin/alice` as a stub that prints `alice 0.1.0` for `--version`, and for `token rotate --show`
    writes a new token to `$ALICE_HOME/token` and prints it.

  The stubs log their arguments to `$R/calls.log`:
  - `useradd` and `userdel` record users in `$R/users`; `id -u <user>` exits 0 only for users in `$R/users`;
  - `chown` does nothing;
  - `systemctl`, on `enable --now alice@<n>`, writes a fake token `tok-<n>-0123456789abcdef0123456789abcdef` to
    `$R/srv/alice/<n>/token`; everything else just logs;
  - `caddy` with `validate` exits with `$CADDY_FAIL` (default 0);
  - `curl` exits with `$PING_FAIL` (default 0);
  - `ss` prints listeners from `$R/listening`;
  - `sudo` drops `-u <user>` and runs the rest;
  - `flock` exits 0;
  - `df` prints a `-Pk` line with `$FREE_KB` available (default 20000000).

  Scenarios, each a function that asserts with `grep` and `test`, printing `ok <name>` or failing with a message:
  1. **`add_creates_everything`:**
     - `alice-server add ann` exits 0 and prints `https://203.0.113.7/u/ann`, the token once (`grep -c` gives 1) and
       `alice remote set https://203.0.113.7/u/ann`;
     - `$R/etc/alice/tenants/ann.env` holds `ALICE_SERVER_PORT=47801`, `ALICE_BACKUP_DAILY=2` and
       `ALICE_BACKUP_WEEKLY=0`, and no `tok-`;
     - the unit file holds `MemoryMax=3G`, `IPAddressAllow=localhost`, the exact `IPAddressDeny=` line and
       `ExecStart=/usr/local/bin/alice daemon run --tenant`;
     - the Caddyfile holds `admin unix//run/caddy/admin.sock`, `default_sni 203.0.113.7`, `profile shortlived` and
       `import /etc/caddy/alice.d/*.caddy`;
     - `$R/etc/caddy/alice.d/ann.caddy` holds `handle_path /u/ann/*`, `127.0.0.1:47801`, `header_up Host {upstream_hostport}`
       and `flush_interval -1`;
     - nothing under `$R/etc` matches `fail2ban`, and `grep -r tok- $R/etc` finds nothing.
  2. **`add_is_idempotent`:** a second `add ann` exits 0, says `exists`, prints no token, and the env file is unchanged
     (`cmp`).
  3. **`ports_skip_used`:** with `127.0.0.1:47802` in `$R/listening`, `add bob` gets 47803.
  4. **`bad_names_are_refused`:** `add Ann`, `add -x`, `add a_b` and a 17-character name each exit non-zero, and nothing is
     created.
  5. **`caddy_failure_rolls_back`:** `CADDY_FAIL=1 alice-server add carl` exits non-zero; no `carl.env`, no `carl.caddy`,
     and `userdel alice-carl` is in `calls.log`.
  6. **`rotate_prints_a_new_token`:** `rotate ann` prints a token different from the first, once.
  7. **`list_shows_and_warns`:** `list` shows `ann` with port 47801 and the URL. With `FREE_KB=2000000` it prints a line
     containing `WARNING` and `3 GB`.
  8. **`upgrade_rolls_back_on_a_failed_ping`:** `PING_FAIL=1 alice-server upgrade $R/new-alice` exits non-zero, and
     `$R/usr/local/bin/alice` is the old stub again (`cmp` with a saved copy).
  9. **`remove_archives_then_deletes`:** `remove ann --yes` leaves a `$R/srv/alice/backups/ann-*.tar.gz` and no
     `$R/srv/alice/ann`, `ann.env` or `ann.caddy`.

  The runner prints `all N scenarios passed` and exits 0, or exits 1 at the first failure.

- [ ] **Step 2: Run to see it fail.** Run: `bash deploy/server/tests/run.sh`. Expected: FAIL (`alice-server` missing).
- [ ] **Step 3: Write the script.** Its structure is fixed; the content follows spec §2.1–§2.5 and §2.7:

```bash
#!/usr/bin/env bash
# alice-server — run Alice for a few people on this server: one daemon, Linux user, port and token per person.
# Spec: docs/superpowers/specs/2026-10-15-alice-server-mode-design.md §2. Run as root (sudo).
set -euo pipefail

R="${ALICE_SERVER_ROOT:-}"                      # tests only: a prefix for every path below
ETC="$R/etc/alice"; TENANTS="$ETC/tenants"; SRV="$R/srv/alice"; ARCHIVES="$SRV/backups"
UNIT="$R/etc/systemd/system/alice@.service"
CADDYFILE="$R/etc/caddy/Caddyfile"; CADDY_D="$R/etc/caddy/alice.d"
BIN="$R/usr/local/bin/alice"; LOCK="$R/run/alice-server.lock"
PORT_MIN=47801; PORT_MAX=47899
WARN_FREE_KB=$((3 * 1024 * 1024))
DENY="10.0.0.0/8 172.16.0.0/12 192.168.0.0/16 169.254.0.0/16 100.64.0.0/10 fc00::/7 fe80::/10"

die() { printf 'alice-server: %s\n' "$*" >&2; exit 1; }
say() { printf '%s\n' "$*"; }
usage() { cat <<'EOF'
usage: alice-server add <name> | remove <name> [--yes] | rotate <name> | list | upgrade <binary>
EOF
}
valid_name() { [[ "$1" =~ ^[a-z][a-z0-9-]{0,15}$ ]] || die "\"$1\" is not a tenant name: lower-case letters, digits and -, starting with a letter, at most 16 characters"; }
public_host() { # from /etc/alice/server.env
  [[ -f "$ETC/server.env" ]] || die "write ALICE_PUBLIC_HOST=<the server's public IP or name> to $ETC/server.env first"
  # shellcheck disable=SC1091
  ( . "$ETC/server.env"; [[ -n "${ALICE_PUBLIC_HOST:-}" ]] || exit 1; printf '%s' "$ALICE_PUBLIC_HOST" ) || die "ALICE_PUBLIC_HOST is empty in $ETC/server.env"
}
url() { printf 'https://%s/u/%s' "$(public_host)" "$1"; }
port_of() { sed -n 's/^ALICE_SERVER_PORT=//p' "$TENANTS/$1.env"; }
free_port() { … lowest PORT_MIN..PORT_MAX not in any $TENANTS/*.env and not in `ss -ltnH` output … }
wait_ping() { # port seconds
  local port=$1 deadline=$((SECONDS + $2))
  until curl -fsS -m 3 -H "Host: 127.0.0.1:$port" "http://127.0.0.1:$port/api/v1/ping" >/dev/null 2>&1; do
    (( SECONDS < deadline )) || return 1; sleep 2
  done
}
write_unit() { … heredoc of spec §2.1 with MemoryMax=3G, IPAddressAllow=localhost, IPAddressDeny=$DENY … }
write_caddyfile() { … once: cp the existing file to Caddyfile.pre-alice if that copy does not exist; then the template of spec §2.3 with admin unix//run/caddy/admin.sock, default_sni <host>, the shortlived issuer, the JSON access log and `import /etc/caddy/alice.d/*.caddy` … }
write_snippet() { … spec §2.3 snippet for <name> <port> … }
reload_caddy() { caddy validate --config "$R/etc/caddy/Caddyfile" --adapter caddyfile >/dev/null && systemctl reload caddy; }
ensure_base() { install -d -m 0755 "$ETC" "$TENANTS" "$SRV" "$CADDY_D" "$(dirname "$UNIT")"; install -d -m 0700 "$ARCHIVES"; write_unit; write_caddyfile; systemctl daemon-reload; }
free_kb() { df -Pk "$SRV" | awk 'NR==2 {print $4}'; }

cmd_add() { … spec §2.5 add, with an undo list run in reverse on any failure (trap) … }
cmd_remove() { … confirm unless --yes; systemctl disable --now; rm snippet; reload_caddy; tar -czf "$ARCHIVES/<name>-<UTC>.tar.gz" -C "$SRV" "<name>" (umask 077); userdel; rm -rf "${home:?}"; rm env … }
cmd_rotate() { … sudo -u "alice-$1" env ALICE_HOME="$SRV/$1" ALICE_NO_AUTOSTART=1 "$BIN" token rotate --show | tail -n 1; print once with the remote set line … }
cmd_list() { … printf table: name port state version url size; then "free: X GB on <fs>; remove archives: Y MB"; WARNING line below 3 GB … }
cmd_upgrade() { … "$1" --version; cp -p "$BIN" "$BIN.prev"; install -m 0755 "$1" "$BIN.new" && mv "$BIN.new" "$BIN"; print free space; restart each alice@<n> and wait_ping 120; on any failure: mv "$BIN.prev" "$BIN", restart all, die naming the failed tenants … }

main() {
  [[ $# -ge 1 ]] || { usage; exit 2; }
  mkdir -p "$(dirname "$LOCK")"; exec 9>"$LOCK"; flock -n 9 || die "another alice-server is running"
  case "$1" in
    add) [[ $# -eq 2 ]] || { usage; exit 2; }; valid_name "$2"; cmd_add "$2" ;;
    remove) [[ $# -ge 2 ]] || { usage; exit 2; }; valid_name "$2"; cmd_remove "$2" "${3:-}" ;;
    rotate) [[ $# -eq 2 ]] || { usage; exit 2; }; valid_name "$2"; cmd_rotate "$2" ;;
    list) cmd_list ;;
    upgrade) [[ $# -eq 2 ]] || { usage; exit 2; }; cmd_upgrade "$2" ;;
    -h|--help|help) usage ;;
    *) usage; exit 2 ;;
  esac
}
main "$@"
```

  Write every `…` body in full. `systemctl` and `caddy` paths in the unit and Caddyfile templates are the real ones
  (`/usr/local/bin/alice`, `/var/log/caddy/alice-access.log`, `/etc/caddy/alice.d/*.caddy`), never prefixed with `$R`:
  the prefix applies only to where files are written. The token is read from `$SRV/<name>/token` and printed once; it is
  never written anywhere else.

- [ ] **Step 4: Run the tests and shellcheck.** Run: `bash deploy/server/tests/run.sh` and
  `shellcheck deploy/server/alice-server deploy/server/tests/run.sh`. Expected: `all 9 scenarios passed`; shellcheck clean,
  or recorded as skipped when it is not installed.
- [ ] **Step 5: Commit**

```bash
git add deploy/server/alice-server deploy/server/tests/run.sh
git commit -m "feat(deploy): alice-server — add, remove, rotate, list, upgrade (R13-R15, R18, R23)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 18: Documentation

**Files:**
- Create: `docs/server.md`
- Modify: `README.md` (a short "Use Alice from a server" section that points to `docs/server.md`), `README.th.md` (a Thai section with the same content as the README pointer, plus the person's steps)

**Content of `docs/server.md`** (English, plain, in this order):
1. **What it is:** one daemon per person; the URL shape; the trust model (Q1: the server's admin can read every tenant's
   memory and key).
2. **For the admin:**
   - one-time setup (`/etc/alice/server.env`; installing the script; Caddy being managed and its original kept as
     `Caddyfile.pre-alice`);
   - `add`, `remove`, `rotate`, `list` and `upgrade`, with example output (tokens shown as `<token>`);
   - the disk budget and what to do below 3 GB (`ALICE_BACKUP_DAILY=1`; delete old `remove` archives; off-machine copies
     are the admin's job);
   - why there is no fail2ban here (the NAT), with the example filter of spec §2.4 for an admin whose NAT preserves
     client addresses;
   - R18 (no LAN access from a tenant) and which settings a tenant cannot change.
3. **For a person:**
   - `alice remote set <url>` (the token is pasted when asked), `alice remote status`, `alice remote unset`;
   - what works and what is refused (spec §3.6);
   - `.alice.toml` works as locally, and redaction happens on your machine;
   - offline writes queue;
   - importing your MemPalace palace (`alice import mempalace`; what is held back in block mode; dry runs);
   - turning Jev on for yourself (`alice config set-secret typesafe`, `alice config set judge.provider jev`).
4. **Troubleshooting:** each `Unavailable` message, what it means, and what to do.

- [ ] **Step 1: Write the three files.**
- [ ] **Step 2: Check every command named in the docs against the real CLI.** Run each one with `--help`, using the
  `alice` binary from `cargo build -p alice-memory`. Every flag mentioned must exist.
- [ ] **Step 3: Commit**

```bash
git add docs/server.md README.md README.th.md
git commit -m "docs: server mode — admin and person guides, disk budget, trust model (EN + Thai section)

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 19: Merge the lane heads again, full gates, whole-branch review

This task is run by the lane's controller session, not by an implementer subagent.

- [ ] **Step 1:** `git fetch origin`, then `git merge --no-edit origin/feat/m2-judge-settings origin/feat/m3-import`. Resolve
  conflicts in favour of keeping both sides' behaviour, and re-run the affected tests.
- [ ] **Step 2: The gates.**
  - `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -j 4 -- -D warnings` and
    `cargo test --workspace -j 4` (throttled prefix, one run at a time);
  - `bash deploy/server/tests/run.sh`;
  - shellcheck, if installed.
- [ ] **Step 3: The whole-branch review** (superpowers:requesting-code-review). The reviewer runs on opus after
  2026-10-16 17:00 Asia/Bangkok, and on sonnet before that only if the controller says so. Fix what it finds, one task per
  finding, as in SDD.
- [ ] **Step 4:** Push `feat/server-mode`. Report to the controller (pane 3 reports to pane 1) and **stop**.

---

### Task 20: Deployment — STOP: only after the controller's explicit go

This task touches the server. Do not start it without the controller's go in this session. Follow spec §8.3, steps 1–10,
exactly:
- the server is reached with `ssh -i ~/.ssh/alice_server_ed25519 -o IdentitiesOnly=yes alice@<server-ip>`, where `<server-ip>` is in the controller's git-ignored notes (never in a committed file);
- never change SSH, sudoers or the firewall;
- record `df -h` before and after the build;
- **only the binary stays** after the build (R25);
- test tenants t1 and t2 are removed with their archives;
- the four real tenants (`icue`, `joe-hermes`, `joe`, `petch`) are created only with the controller's go, and token
  handover is the controller's.

Report every measurement (RAM, disk before and after each step, RSS per tenant) to the controller.
