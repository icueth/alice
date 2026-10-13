# Alice M1 — Store, Search, Interfaces — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship Alice milestone M1: a single `alice` binary whose daemon owns a SQLite memory store with hybrid (vector + BM25 + trigram + code-identifier) search, exposed to agents through 7 MCP tools (stdio shim and Streamable HTTP), a CLI and a JSON HTTP API, meeting the spec's latency budgets without ever blocking a client.

**Architecture:** Rust workspace with four crates. `alice-core` holds the domain model, SQLite store (schema, migrations, FTS5), search ranking and KG logic as synchronous, unit-testable code. `alice-embed` wraps embedding providers behind a trait (fastembed ONNX in production, a deterministic fake in tests). `alice-daemon` runs one writer thread (short batched transactions), an embedding/enrichment worker, a read-connection pool, the axum HTTP server (JSON API + MCP) and the shared `Service` layer. `alice` (package `alice-memory`) is the single binary: CLI, MCP stdio shim, daemon entry point and client with auto-start and spool fallback.

**Tech Stack:** Rust 1.99 (edition 2024), rusqlite 0.40 (bundled SQLite, FTS5), usearch 2.26, fastembed 7.1 (ONNX Runtime), rmcp 3.5, axum 0.8, tokio 1.53, clap 4.6, serde, schemars 1.2, chrono 0.4, ulid 3.0, sha2 0.11.

**Spec:** `docs/superpowers/specs/2026-10-13-alice-core-design.md` (sections 2, 3, 4, 5.1 steps 1–5 without Judge, 5.7, 8, 10, 12-M1).

## Global Constraints

- Toolchain: Rust stable 1.99, edition 2024, `resolver = "3"`; pinned in `rust-toolchain.toml`.
- Crates: `alice-core`, `alice-embed`, `alice-daemon`, and the binary package `alice-memory` with `[[bin]] name = "alice"`. All `license = "MIT"`, `repository = "https://github.com/icueth/alice"`.
- Data directory: `ALICE_HOME` env var, default `~/.alice` on every OS. Layout per spec §2.3.
- IDs: ULID, 26-char uppercase strings. Timestamps: RFC 3339 UTC with milliseconds and `Z` (e.g. `2026-10-13T09:44:23.391Z`), stored as TEXT. HLC strings: `{millis:013}-{counter:04x}-{device}`.
- Performance budgets (spec §4.6): `remember` ack p95 < 50 ms; `recall` fast p95 < 200 ms at 1M chunks; `context` p95 < 150 ms; `alice mcp` start-up < 20 ms; daemon RSS < 800 MB at 1M chunks.
- Only the daemon opens `alice.db`. Clients never touch the database file.
- The writer thread only runs short transactions; embedding, parsing and bulk work happen on other threads.
- Limits: memory content ≤ 65,536 bytes; title ≤ 200 chars; ≤ 20 topics of ≤ 64 chars; importance 1–5; chunk text ≤ 32,768 bytes; ≤ 1,000 chunks per ingest request; `recall` limit default 10, max 50; `max_tokens` default 1,500 for `recall`, 1,200 for `context`.
- Server binds 127.0.0.1 only; every API/MCP request needs `Authorization: Bearer <token>`; `Host` must be `127.0.0.1:<port>` or `localhost:<port>`; any `Origin` must equal the daemon origin.
- Error text returned to agents: one or two sentences saying what happened and what to do next.
- MCP tool names exactly: `recall`, `remember`, `context`, `get_memory`, `update_memory`, `query_facts`, `assert_fact`.
- Build and test with throttling on the owner's machine: `nice -n 15 cargo build -j 6`, `nice -n 15 cargo test -j 6`; heavy runs one at a time. Run `source "$HOME/.cargo/env"` first in every shell.
- CI: GitHub Actions on macOS, Linux, Windows: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
- Commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. The repo's commit identity is already configured; do not change it.

## Review Focus

1. **Query text containing FTS5 syntax** (`"`, `*`, `-`, `:`, `(`, `NEAR`, `AND`, a lone `"`) must never raise an FTS syntax error; it is searched as literal words. Test owned by Task 8.
2. **Short or Thai-only queries** (`AI`, `ไทย`, `ความจำ`, a single emoji) must return results or an empty list, never an error; trigram is skipped for terms under 3 characters and vector search still runs. Test owned by Task 8.
3. **Stale `daemon.json` or a racing first start** (daemon killed, port now used by another process; ten clients starting at once) must not hang, must not send the token to a foreign process, must start exactly one daemon and lose no writes. Tests owned by Tasks 11 and 13.
4. **Oversized input** (a 200 KB `remember`, an ingest with 5,000 chunks, a 40 KB chunk) gets an actionable `Invalid` error naming the limit; nothing partial is written. Test owned by Task 4.
5. **Unusual working directories** (git worktree, repo without remote, non-git directory, `$HOME`, a deleted directory) resolve to a sensible project (same project for a worktree and its main checkout; `global` for `$HOME`; error-free fallback when the directory is gone). Test owned by Task 3.

---

## File Structure

```
Cargo.toml                         workspace manifest, shared dependency versions
rust-toolchain.toml                pins 1.99
.github/workflows/ci.yml           fmt + clippy + test on 3 OSes
crates/alice-core/
  Cargo.toml
  src/lib.rs                       module declarations, re-exports
  src/error.rs                     AliceError, Result
  src/ids.rs                       new_id(), now_rfc3339(), Hlc
  src/model.rs                     Kind, Status, Author, Memory, SourceType, limits
  src/normalize.rs                 slugify, normalize_topic, nfc, content_hash, estimate_tokens, truncate_chars
  src/codetokens.rs                expand_code_tokens, split_camel
  src/project.rs                   ProjectRef, detect_project, normalize_remote
  src/api.rs                       request/response types shared by HTTP, MCP and CLI
  src/store/mod.rs                 open_writer, open_reader, configure, ReaderPool
  src/store/migrations.rs          migration runner + pre-migration backup
  src/store/sql/0001_init.sql      full M1 schema
  src/store/fts.rs                 FTS row maintenance
  src/store/projects.rs            upsert/resolve projects and aliases
  src/store/memories.rs            prepare/insert/get/update memories
  src/store/sources.rs             sources + chunks ingest
  src/store/kg.rs                  entities, predicates, relations, mentions
  src/store/embeddings.rs          f16 vector blobs, pending queues
  src/search/mod.rs                Searcher (recall pipeline)
  src/search/lexical.rs            FTS query building + candidate queries
  src/search/fusion.rs             RRF + boosts (pure)
  src/search/pack.rs               text/json packing to a token budget
  src/vector.rs                    VectorIndex over usearch, key encoding
  src/context.rs                   project briefing
crates/alice-embed/
  src/lib.rs                       Embedder trait, FakeEmbedder, FastEmbedder
crates/alice-daemon/
  src/lib.rs
  src/paths.rs                     AliceHome layout
  src/writer.rs                    writer thread, WriteOp, group commit
  src/worker.rs                    embedding + enrichment worker
  src/service.rs                   Service: all operations
  src/http.rs                      axum router, auth middleware, JSON handlers
  src/mcp.rs                       MCP server (tools, instructions, prompts, resources) over Backend
  src/daemon.rs                    run(): lock, token, daemon.json, spool drain, shutdown
  src/spool.rs                     spool file format + drain
crates/alice/
  Cargo.toml                       package alice-memory, bin alice
  src/main.rs                      clap entry
  src/client.rs                    daemon discovery, auto-start, HTTP calls, spool fallback
  src/shim.rs                      `alice mcp` stdio server with HttpBackend
  src/cmd.rs                       subcommand implementations and output formatting
  tests/cli.rs                     process-level tests (start race, kill -9, CLI round trips)
tests/fixtures/search_gold.jsonl   multilingual gold queries (in alice-core/tests/)
```

---

### Task 1: Workspace and core primitives

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `.github/workflows/ci.yml`
- Create: `crates/alice-core/Cargo.toml`, `crates/alice-core/src/{lib.rs,error.rs,ids.rs,model.rs,normalize.rs,codetokens.rs}`
- Test: unit tests inside each module (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `alice_core::error::{AliceError, Result}`
  - `alice_core::ids::{new_id() -> String, now_rfc3339() -> String, Hlc::{new(device: impl Into<String>) -> Hlc, tick(&mut self) -> String, tick_at(&mut self, now_ms: i64) -> String, observe(&mut self, remote: &str)}, parse_hlc(&str) -> Option<(i64, u32, String)>}`
  - `alice_core::model::{Kind, Status, Author, SourceType, Memory, limits::*}` with `Kind::{as_str, parse, half_life_days, ALL}`, `Status::{as_str, parse}`
  - `alice_core::normalize::{nfc(&str) -> String, slugify(&str) -> String, normalize_topic(&str) -> Option<String>, content_hash(project: &str, kind: Kind, title: &str, body: &str) -> String, sha256_hex(&[u8]) -> String, estimate_tokens(&str) -> usize, truncate_chars(&str, usize) -> String, derive_title(&str) -> String}`
  - `alice_core::codetokens::{expand_code_tokens(&str) -> String, split_camel(&str) -> Vec<String>, is_codeish(&str) -> bool}`

- [ ] **Step 1: Create the workspace manifest and toolchain pin**

`Cargo.toml`:
```toml
[workspace]
resolver = "3"
members = ["crates/alice-core"]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "MIT"
repository = "https://github.com/icueth/alice"
rust-version = "1.99"
authors = ["icueth and Alice contributors"]

[workspace.dependencies]
anyhow = "1.0.104"
thiserror = "2.0.21"
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.154"
schemars = "1.2.3"
chrono = { version = "0.4.45", default-features = false, features = ["clock", "std"] }
ulid = "3.0.0"
sha2 = "0.11.1"
hex = "0.4.3"
unicode-normalization = "0.1.25"
regex = "1.13.1"
parking_lot = "0.12.5"
rusqlite = { version = "0.40.2", features = ["bundled"] }
tracing = "0.1.44"
tempfile = "3.27.0"

[profile.release]
lto = "thin"
codegen-units = 4
strip = "symbols"
```

`rust-toolchain.toml`:
```toml
[toolchain]
channel = "1.99"
components = ["rustfmt", "clippy"]
```

`crates/alice-core/Cargo.toml`:
```toml
[package]
name = "alice-core"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
rust-version.workspace = true
description = "Core model, storage and search for Alice, long-term memory for AI coding agents"

[dependencies]
thiserror.workspace = true
serde.workspace = true
serde_json.workspace = true
schemars.workspace = true
chrono.workspace = true
ulid.workspace = true
sha2.workspace = true
hex.workspace = true
unicode-normalization.workspace = true
regex.workspace = true
parking_lot.workspace = true
rusqlite.workspace = true
tracing.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

- [ ] **Step 2: Write `error.rs`**

```rust
//! Error type shared by every Alice crate.

/// Errors carry messages written for the agent or person who will read them:
/// what happened and what to do next.
#[derive(Debug, thiserror::Error)]
pub enum AliceError {
    /// The caller sent something invalid; the message says how to fix it.
    #[error("{0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(String),
    /// Writes are refused until the cause is fixed.
    #[error("memory is read-only: {0}. Run \"alice doctor\".")]
    ReadOnly(String),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, AliceError>;

impl AliceError {
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::Invalid(msg.into())
    }
}
```

- [ ] **Step 3: Write failing tests for ids, normalize and codetokens**

Append to each module file (create the files with only the tests and empty `pub fn` stubs that `todo!()` so they compile):

`ids.rs` tests:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_26_char_and_sortable() {
        let a = new_id();
        let b = new_id();
        assert_eq!(a.len(), 26);
        assert!(b > a, "monotonic ids must sort in creation order: {a} {b}");
    }

    #[test]
    fn rfc3339_has_millis_and_z() {
        let t = now_rfc3339();
        assert!(t.ends_with('Z'), "{t}");
        assert_eq!(t.len(), "2026-10-13T09:44:23.391Z".len(), "{t}");
    }

    #[test]
    fn hlc_orders_within_same_millisecond() {
        let mut h = Hlc::new("dev1");
        let a = h.tick_at(1_000);
        let b = h.tick_at(1_000);
        let c = h.tick_at(999); // clock went backwards
        let d = h.tick_at(2_000);
        assert!(a < b && b < c && c < d, "{a} {b} {c} {d}");
        assert_eq!(a, "0000000001000-0000-dev1");
        assert_eq!(parse_hlc(&c), Some((1_000, 2, "dev1".to_string())));
    }

    #[test]
    fn hlc_observe_moves_forward() {
        let mut h = Hlc::new("a");
        h.observe("0000000005000-0003-b");
        let next = h.tick_at(10);
        assert_eq!(next, "0000000005000-0004-a");
    }
}
```

`normalize.rs` tests:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;

    #[test]
    fn slugify_folds_case_and_separators() {
        assert_eq!(slugify("PG_AI"), "pg-ai");
        assert_eq!(slugify("pg-ai"), "pg-ai");
        assert_eq!(slugify("  My App!! "), "my-app");
        assert_eq!(slugify("tara.stock_market"), "tara-stock-market");
        assert_eq!(slugify("wing_pg_ai"), "wing-pg-ai");
        assert_eq!(slugify("---"), "");
    }

    #[test]
    fn slugify_keeps_thai_marks() {
        assert_eq!(slugify("ระบบ สต็อก"), "ระบบ-สต็อก");
        assert_eq!(slugify("ไม้เอก่"), "ไม้เอก่");
    }

    #[test]
    fn topics_are_slugs_with_limits() {
        assert_eq!(normalize_topic(" Bug Fix "), Some("bug-fix".to_string()));
        assert_eq!(normalize_topic("!!!"), None);
        assert_eq!(normalize_topic(&"a".repeat(80)).unwrap().chars().count(), 64);
    }

    #[test]
    fn content_hash_ignores_whitespace_and_composition() {
        // "ำ" (U+0E33) vs "ํา" (U+0E4D U+0E32) normalise to the same NFC text.
        let a = content_hash("p", Kind::Note, "t", "จำ  ไว้\n");
        let b = content_hash("p", Kind::Note, "t", "จำ ไว้");
        assert_eq!(a, b);
        assert_ne!(a, content_hash("p", Kind::Fact, "t", "จำ ไว้"));
        assert_ne!(a, content_hash("q", Kind::Note, "t", "จำ ไว้"));
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn derive_title_uses_first_line() {
        assert_eq!(derive_title("  first line\nsecond"), "first line");
        let long = "ก".repeat(300);
        assert_eq!(derive_title(&long).chars().count(), 80);
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate_chars("สวัสดีครับ", 3), "สวั…");
        assert_eq!(truncate_chars("abc", 5), "abc");
    }

    #[test]
    fn token_estimate_is_bytes_over_four() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }
}
```

`codetokens.rs` tests:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        expand_code_tokens(s).split(' ').filter(|w| !w.is_empty()).map(String::from).collect()
    }

    #[test]
    fn camel_case_is_split() {
        let w = words("call HostLinkReconnect now");
        for want in ["hostlinkreconnect", "host", "link", "reconnect"] {
            assert!(w.contains(&want.to_string()), "{want} missing from {w:?}");
        }
    }

    #[test]
    fn snake_case_and_paths_are_split() {
        let w = words("writer_lease in internal/teacher/jev/client.go");
        for want in ["writer", "lease", "internal", "teacher", "jev", "client"] {
            assert!(w.contains(&want.to_string()), "{want} missing from {w:?}");
        }
    }

    #[test]
    fn acronyms_split_before_the_next_word() {
        assert_eq!(split_camel("HTTPServer"), vec!["HTTP", "Server"]);
        assert_eq!(split_camel("v4Frame"), vec!["v4", "Frame"]);
    }

    #[test]
    fn plain_prose_produces_nothing() {
        assert_eq!(expand_code_tokens("plain words here, nothing special."), "");
        assert_eq!(expand_code_tokens("ภาษาไทยล้วน"), "");
    }

    #[test]
    fn output_has_no_duplicates() {
        let w = words("foo_bar foo_bar");
        let mut d = w.clone();
        d.dedup();
        assert_eq!(w.len(), d.len());
    }
}
```

- [ ] **Step 4: Run the tests to confirm they fail**

Run: `source "$HOME/.cargo/env" && nice -n 15 cargo test -j 6 -p alice-core`
Expected: compile succeeds, tests FAIL with `not yet implemented` panics.

- [ ] **Step 5: Implement `ids.rs`**

```rust
//! Identifiers and clocks.

use chrono::{SecondsFormat, Utc};
use std::sync::Mutex;
use ulid::Generator;

// `Generator::new` is const in ulid 3.0, so a static works; the generator itself is not Sync.
static GENERATOR: Mutex<Generator> = Mutex::new(Generator::new());

/// A new ULID. Monotonic within the process, so ids sort in creation order.
/// (`Ulid::generate()` alone is NOT monotonic inside one millisecond.)
pub fn new_id() -> String {
    let mut generator = GENERATOR.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match generator.generate() {
        Ok(id) => id.to_string(),
        // 80 random bits overflowed within one millisecond (practically never).
        Err(overflow) => overflow.commit_overflow_increment().to_string(),
    }
}

/// Current UTC time as RFC 3339 with milliseconds, e.g. `2026-10-13T09:44:23.391Z`.
pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Hybrid logical clock. Strings sort by (milliseconds, counter); the device id breaks ties.
#[derive(Debug, Clone)]
pub struct Hlc {
    device: String,
    last_ms: i64,
    counter: u32,
}

impl Hlc {
    pub fn new(device: impl Into<String>) -> Self {
        Self { device: device.into(), last_ms: 0, counter: 0 }
    }

    pub fn tick(&mut self) -> String {
        self.tick_at(Utc::now().timestamp_millis())
    }

    pub fn tick_at(&mut self, now_ms: i64) -> String {
        if now_ms > self.last_ms {
            self.last_ms = now_ms;
            self.counter = 0;
        } else {
            self.counter += 1;
        }
        format!("{:013}-{:04x}-{}", self.last_ms, self.counter, self.device)
    }

    /// Advance past a timestamp seen from another device.
    pub fn observe(&mut self, remote: &str) {
        if let Some((ms, counter, _)) = parse_hlc(remote) {
            if ms > self.last_ms || (ms == self.last_ms && counter > self.counter) {
                self.last_ms = ms;
                self.counter = counter;
            }
        }
    }
}

pub fn parse_hlc(s: &str) -> Option<(i64, u32, String)> {
    let mut parts = s.splitn(3, '-');
    let ms = parts.next()?.parse().ok()?;
    let counter = u32::from_str_radix(parts.next()?, 16).ok()?;
    let device = parts.next()?.to_string();
    Some((ms, counter, device))
}
```

- [ ] **Step 6: Implement `model.rs`**

```rust
//! Domain types.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub mod limits {
    pub const MAX_CONTENT_BYTES: usize = 65_536;
    pub const MAX_TITLE_CHARS: usize = 200;
    pub const MAX_TOPICS: usize = 20;
    pub const MAX_TOPIC_CHARS: usize = 64;
    pub const MAX_CHUNK_BYTES: usize = 32_768;
    pub const MAX_CHUNKS_PER_REQUEST: usize = 1_000;
    pub const DEFAULT_RECALL_LIMIT: usize = 10;
    pub const MAX_RECALL_LIMIT: usize = 50;
    pub const DEFAULT_RECALL_TOKENS: usize = 1_500;
    pub const DEFAULT_CONTEXT_TOKENS: usize = 1_200;
}

/// What a memory is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Fact,
    Decision,
    Bugfix,
    Howto,
    Preference,
    Warning,
    Status,
    Note,
    Summary,
    Diary,
    Handoff,
}

impl Kind {
    pub const ALL: [Kind; 11] = [
        Kind::Fact, Kind::Decision, Kind::Bugfix, Kind::Howto, Kind::Preference, Kind::Warning,
        Kind::Status, Kind::Note, Kind::Summary, Kind::Diary, Kind::Handoff,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Fact => "fact",
            Kind::Decision => "decision",
            Kind::Bugfix => "bugfix",
            Kind::Howto => "howto",
            Kind::Preference => "preference",
            Kind::Warning => "warning",
            Kind::Status => "status",
            Kind::Note => "note",
            Kind::Summary => "summary",
            Kind::Diary => "diary",
            Kind::Handoff => "handoff",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s.trim().to_ascii_lowercase())
    }

    /// Recency half-life used by ranking; `None` means the kind does not decay.
    pub fn half_life_days(self) -> Option<f64> {
        match self {
            Kind::Status => Some(7.0),
            Kind::Handoff => Some(14.0),
            Kind::Diary => Some(30.0),
            Kind::Note => Some(90.0),
            Kind::Fact | Kind::Howto | Kind::Bugfix | Kind::Summary => Some(180.0),
            Kind::Decision | Kind::Preference | Kind::Warning => None,
        }
    }
}

/// Half-life applied to raw chunks.
pub const RAW_HALF_LIFE_DAYS: f64 = 90.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Active,
    Superseded,
    Archived,
    Trashed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Active => "active",
            Status::Superseded => "superseded",
            Status::Archived => "archived",
            Status::Trashed => "trashed",
        }
    }

    pub fn parse(s: &str) -> Option<Status> {
        [Status::Pending, Status::Active, Status::Superseded, Status::Archived, Status::Trashed]
            .into_iter()
            .find(|v| v.as_str() == s)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    Session,
    File,
    Import,
}

impl SourceType {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceType::Session => "session",
            SourceType::File => "file",
            SourceType::Import => "import",
        }
    }
}

/// Who wrote a memory. Metadata only; never a container.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Author {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// A stored knowledge memory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Memory {
    pub id: String,
    pub project: String,
    pub kind: Kind,
    pub title: String,
    pub body: String,
    pub topics: Vec<String>,
    pub importance: u8,
    pub status: Status,
    pub author: Author,
    pub created_at: String,
    pub updated_at: String,
    pub access_count: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_id: Option<String>,
}
```

- [ ] **Step 7: Implement `normalize.rs`**

```rust
//! Text normalisation shared by storage, dedup and ranking.

use crate::model::{Kind, limits};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

pub fn nfc(s: &str) -> String {
    s.nfc().collect()
}

fn is_separator(ch: char) -> bool {
    ch.is_whitespace()
        || ch.is_control()
        || (ch.is_ascii() && !ch.is_ascii_alphanumeric())
        || matches!(ch, '\u{00B7}' | '\u{2010}'..='\u{2027}' | '\u{3000}'..='\u{303F}')
}

/// Lowercase kebab-case slug. Non-ASCII letters and combining marks (Thai, CJK) are kept.
pub fn slugify(s: &str) -> String {
    let lower = nfc(s).to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut pending_dash = false;
    for ch in lower.chars() {
        if is_separator(ch) {
            pending_dash = !out.is_empty();
        } else {
            if pending_dash {
                out.push('-');
                pending_dash = false;
            }
            out.push(ch);
        }
    }
    out
}

/// Topic tags are slugs capped at 64 characters; `None` when nothing is left.
pub fn normalize_topic(s: &str) -> Option<String> {
    let slug: String = slugify(s).chars().take(limits::MAX_TOPIC_CHARS).collect();
    let slug = slug.trim_end_matches('-').to_string();
    (!slug.is_empty()).then_some(slug)
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Dedup key: same project, kind, title and body (NFC, whitespace-collapsed) => same hash.
pub fn content_hash(project: &str, kind: Kind, title: &str, body: &str) -> String {
    let normalized = format!(
        "{project}\u{1f}{}\u{1f}{}\u{1f}{}",
        kind.as_str(),
        collapse_whitespace(&nfc(title)),
        collapse_whitespace(&nfc(body)),
    );
    sha256_hex(normalized.as_bytes())
}

/// Rough token count used for budgets: one token per four UTF-8 bytes, rounded up.
pub fn estimate_tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

/// Cut to `max` characters, appending `…` when something was removed.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Title for a memory saved without one: its first non-empty line, at most 80 characters.
pub fn derive_title(content: &str) -> String {
    let line = content.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    line.chars().take(80).collect()
}
```

> `truncate_chars("สวัสดีครับ", 3)` must equal `"สวั…"`: `chars()` counts code points, and Thai vowel marks are separate code points, so the expected value in the test is exact.

- [ ] **Step 8: Implement `codetokens.rs`**

```rust
//! Identifier and path expansion so FTS can match code names by their parts.

use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z0-9_][A-Za-z0-9_./:\-]*[A-Za-z0-9_]").expect("static regex")
});

fn has_case_change(tok: &str) -> bool {
    let chars: Vec<char> = tok.chars().collect();
    chars.windows(2).any(|w| (w[0].is_lowercase() || w[0].is_ascii_digit()) && w[1].is_uppercase())
}

/// Looks like an identifier or a path rather than a plain word.
pub fn is_codeish(tok: &str) -> bool {
    tok.contains(['_', '/', '.', ':']) || has_case_change(tok)
}

/// Split camelCase and PascalCase, keeping acronyms together (`HTTPServer` -> `HTTP`, `Server`).
pub fn split_camel(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut parts = Vec::new();
    let mut current = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && !current.is_empty() {
            let prev = chars[i - 1];
            let next_is_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            let boundary = (c.is_uppercase() && (prev.is_lowercase() || prev.is_ascii_digit()))
                || (c.is_uppercase() && prev.is_uppercase() && next_is_lower);
            if boundary {
                parts.push(std::mem::take(&mut current));
            }
        }
        current.push(c);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Lowercase words derived from code-like tokens in `text`, space separated, without duplicates.
/// Plain prose yields an empty string.
pub fn expand_code_tokens(text: &str) -> String {
    let mut seen = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut push = |w: String, out: &mut Vec<String>| {
        if w.chars().count() >= 2 && seen.insert(w.clone()) {
            out.push(w);
        }
    };
    for m in TOKEN.find_iter(text) {
        let tok = m.as_str();
        if tok.len() < 3 || !is_codeish(tok) {
            continue;
        }
        let joined: String = tok.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_lowercase();
        push(joined, &mut out);
        for segment in tok.split(['_', '/', '.', ':', '-']).filter(|s| !s.is_empty()) {
            let pieces = split_camel(segment);
            if pieces.len() > 1 {
                push(segment.to_lowercase(), &mut out);
            }
            for piece in pieces {
                push(piece.to_lowercase(), &mut out);
            }
        }
    }
    out.join(" ")
}
```

- [ ] **Step 9: `lib.rs` and CI**

`crates/alice-core/src/lib.rs`:
```rust
//! Alice core: domain model, storage and search.

pub mod codetokens;
pub mod error;
pub mod ids;
pub mod model;
pub mod normalize;

pub use error::{AliceError, Result};
```

`.github/workflows/ci.yml`:
```yaml
name: ci
on:
  push:
    branches: [main]
  pull_request:
jobs:
  test:
    strategy:
      fail-fast: false
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@master
        with:
          toolchain: "1.99"
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: cargo test --workspace
```

- [ ] **Step 10: Run tests, fmt and clippy**

Run: `source "$HOME/.cargo/env" && nice -n 15 cargo test -j 6 -p alice-core && cargo fmt --all --check && nice -n 15 cargo clippy -j 6 --workspace --all-targets -- -D warnings`
Expected: all tests PASS, no fmt diff, no clippy warnings.

- [ ] **Step 11: Commit**

```bash
git add Cargo.toml rust-toolchain.toml .github crates/alice-core
git commit -m "feat(core): workspace, ids/HLC, model, normalisation, code-token expansion

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: SQLite store foundation (schema, migrations, connections)

**Files:**
- Create: `crates/alice-core/src/store/{mod.rs,migrations.rs}`, `crates/alice-core/src/store/sql/0001_init.sql`
- Modify: `crates/alice-core/src/lib.rs` (add `pub mod store;`)
- Test: `crates/alice-core/src/store/mod.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `ids::now_rfc3339`, `AliceError`
- Produces:
  - `store::open_writer(path: &Path) -> Result<rusqlite::Connection>` (creates file, configures, migrates)
  - `store::open_reader(path: &Path) -> Result<rusqlite::Connection>` (read-only)
  - `store::ReaderPool::{new(path: PathBuf, max: usize) -> ReaderPool, with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T>}`
  - `store::migrations::{migrate(conn: &mut Connection, db_path: &Path) -> Result<usize>, CURRENT_VERSION: i64}`
  - `store::backup_into(conn: &Connection, dest: &Path) -> Result<()>` (uses `VACUUM INTO`)
  - Tables and FTS tables exactly as in `0001_init.sql` below.

- [ ] **Step 1: Write the schema `crates/alice-core/src/store/sql/0001_init.sql`**

```sql
-- Alice schema v1. Timestamps are RFC 3339 UTC strings with milliseconds.

CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE projects (
  id         TEXT PRIMARY KEY,
  key        TEXT NOT NULL UNIQUE,   -- normalized git remote, "path:<root>", or "global"
  slug       TEXT NOT NULL UNIQUE,
  name       TEXT NOT NULL,
  git_remote TEXT,
  root_path  TEXT,
  created_at TEXT NOT NULL
);

CREATE TABLE project_aliases (
  alias      TEXT PRIMARY KEY,       -- slugified
  project_id TEXT NOT NULL REFERENCES projects(id)
);

CREATE TABLE memories (
  seq             INTEGER PRIMARY KEY,   -- rowid; FTS and vector keys use it
  id              TEXT NOT NULL UNIQUE,
  project_id      TEXT NOT NULL REFERENCES projects(id),
  kind            TEXT NOT NULL,
  title           TEXT NOT NULL,
  body            TEXT NOT NULL,
  importance      INTEGER NOT NULL DEFAULT 3 CHECK (importance BETWEEN 1 AND 5),
  status          TEXT NOT NULL,
  content_hash    TEXT NOT NULL,
  author_agent    TEXT,
  author_model    TEXT,
  author_session  TEXT,
  idempotency_key TEXT,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL,
  hlc             TEXT NOT NULL,
  access_count    INTEGER NOT NULL DEFAULT 0,
  last_accessed_at TEXT,
  legacy_id       TEXT,
  embed_state     TEXT NOT NULL DEFAULT 'pending',  -- pending | done | retry | failed
  judge_state     TEXT NOT NULL DEFAULT 'pending'   -- pending | done | retry | skipped
);
CREATE INDEX memories_project_kind ON memories(project_id, kind, status, created_at);
CREATE UNIQUE INDEX memories_hash ON memories(project_id, content_hash) WHERE status <> 'trashed';
CREATE UNIQUE INDEX memories_idem ON memories(idempotency_key) WHERE idempotency_key IS NOT NULL;
CREATE INDEX memories_embed ON memories(embed_state) WHERE embed_state <> 'done';
CREATE INDEX memories_legacy ON memories(legacy_id) WHERE legacy_id IS NOT NULL;

CREATE TABLE memory_topics (
  memory_id TEXT NOT NULL,
  topic     TEXT NOT NULL,
  PRIMARY KEY (memory_id, topic)
);
CREATE INDEX memory_topics_topic ON memory_topics(topic);

CREATE TABLE sources (
  id          TEXT PRIMARY KEY,
  project_id  TEXT NOT NULL REFERENCES projects(id),
  type        TEXT NOT NULL,           -- session | file | import
  agent       TEXT,
  uri         TEXT NOT NULL,
  session_id  TEXT,
  started_at  TEXT,
  ended_at    TEXT,
  legacy_ref  TEXT,
  created_at  TEXT NOT NULL,
  UNIQUE (type, uri)
);

CREATE TABLE chunks (
  seq          INTEGER PRIMARY KEY,
  id           TEXT NOT NULL UNIQUE,
  source_id    TEXT NOT NULL REFERENCES sources(id),
  project_id   TEXT NOT NULL REFERENCES projects(id),
  ord          INTEGER NOT NULL,
  text         TEXT NOT NULL,
  content_hash TEXT NOT NULL,
  authored_at  TEXT,
  created_at   TEXT NOT NULL,
  embed_state  TEXT NOT NULL DEFAULT 'pending',
  legacy_id    TEXT,
  UNIQUE (source_id, ord)
);
CREATE INDEX chunks_embed ON chunks(embed_state) WHERE embed_state <> 'done';
CREATE INDEX chunks_project ON chunks(project_id, created_at);

CREATE TABLE provenance (
  memory_id TEXT NOT NULL,
  chunk_id  TEXT NOT NULL,
  PRIMARY KEY (memory_id, chunk_id)
);

CREATE TABLE links (
  from_id    TEXT NOT NULL,
  to_id      TEXT NOT NULL,
  type       TEXT NOT NULL,   -- relates_to | supersedes | contradicts | derived_from | duplicate_of
  created_by TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (from_id, to_id, type)
);
CREATE INDEX links_to ON links(to_id);

CREATE TABLE entities (
  id         TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  type       TEXT,
  created_at TEXT NOT NULL
);
CREATE TABLE entity_aliases (
  alias     TEXT PRIMARY KEY,   -- lowercase NFC, whitespace-collapsed
  entity_id TEXT NOT NULL REFERENCES entities(id)
);
CREATE TABLE predicates (
  name        TEXT PRIMARY KEY,
  description TEXT,
  canonical   TEXT              -- NULL when this name is canonical
);
CREATE TABLE relations (
  id               TEXT PRIMARY KEY,
  subject_id       TEXT NOT NULL REFERENCES entities(id),
  predicate        TEXT NOT NULL,
  object_entity_id TEXT REFERENCES entities(id),
  object_literal   TEXT,
  valid_from       TEXT NOT NULL,
  valid_to         TEXT,
  source_memory_id TEXT,
  created_at       TEXT NOT NULL,
  hlc              TEXT NOT NULL,
  CHECK ((object_entity_id IS NULL) <> (object_literal IS NULL))
);
CREATE INDEX relations_subject ON relations(subject_id, predicate);
CREATE INDEX relations_object ON relations(object_entity_id);
CREATE TABLE mentions (
  memory_id TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  PRIMARY KEY (memory_id, entity_id)
);
CREATE INDEX mentions_entity ON mentions(entity_id);

CREATE TABLE embeddings (
  owner_type TEXT NOT NULL,     -- memory | chunk
  owner_seq  INTEGER NOT NULL,
  model      TEXT NOT NULL,
  dim        INTEGER NOT NULL,
  vector     BLOB NOT NULL,     -- little-endian f16
  PRIMARY KEY (owner_type, owner_seq, model)
);

CREATE TABLE events (
  id          TEXT PRIMARY KEY,
  ts          TEXT NOT NULL,
  hlc         TEXT NOT NULL,
  actor       TEXT NOT NULL,
  action      TEXT NOT NULL,
  target_type TEXT NOT NULL,
  target_id   TEXT NOT NULL,
  before_json TEXT,
  after_json  TEXT
);
CREATE TABLE review_items (
  id           TEXT PRIMARY KEY,
  type         TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  confidence   REAL,
  status       TEXT NOT NULL,
  created_at   TEXT NOT NULL,
  resolved_at  TEXT,
  resolved_by  TEXT,
  resolution   TEXT
);
CREATE TABLE judge_cache (
  request_hash  TEXT PRIMARY KEY,
  provider      TEXT NOT NULL,
  model         TEXT NOT NULL,
  response_json TEXT NOT NULL,
  created_at    TEXT NOT NULL
);
CREATE TABLE cost_ledger (
  day           TEXT NOT NULL,
  provider      TEXT NOT NULL,
  calls         INTEGER NOT NULL DEFAULT 0,
  input_tokens  INTEGER NOT NULL DEFAULT 0,
  output_tokens INTEGER NOT NULL DEFAULT 0,
  usd           REAL NOT NULL DEFAULT 0,
  PRIMARY KEY (day, provider)
);
CREATE TABLE sync_state (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- Full-text indexes. Contentless: rows are maintained by store::fts, rowid = memories.seq / chunks.seq.
CREATE VIRTUAL TABLE memories_fts USING fts5(
  title, body, topics, code_tokens,
  content = '', contentless_delete = 1,
  tokenize = 'unicode61 remove_diacritics 2'
);
CREATE VIRTUAL TABLE memories_tri USING fts5(
  title, body,
  content = '', contentless_delete = 1,
  tokenize = 'trigram'
);
CREATE VIRTUAL TABLE chunks_fts USING fts5(
  text, code_tokens,
  content = '', contentless_delete = 1,
  tokenize = 'unicode61 remove_diacritics 2'
);
CREATE VIRTUAL TABLE chunks_tri USING fts5(
  text,
  content = '', contentless_delete = 1,
  tokenize = 'trigram'
);

INSERT INTO projects (id, key, slug, name, created_at)
VALUES ('00000000000000000000GLOBAL', 'global', 'global', 'Global', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
```

> rusqlite 0.40.2 `bundled` ships SQLite 3.53.2, which supports `contentless_delete` (3.43+). `seq INTEGER PRIMARY KEY` is a rowid alias, so it survives `VACUUM` and FTS rowids stay in sync. A trigram `MATCH` with fewer than 3 code points returns 0 rows (not an error); Task 8 adds a `LIKE` fallback for short non-ASCII terms.

- [ ] **Step 2: Write failing tests in `store/mod.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alice.db");
        (dir, path)
    }

    #[test]
    fn writer_creates_schema_in_wal_mode() {
        let (_d, path) = temp_db();
        let conn = open_writer(&path).unwrap();
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
        let n: i64 = conn
            .query_row("SELECT count(*) FROM projects WHERE slug = 'global'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn migrations_are_idempotent() {
        let (_d, path) = temp_db();
        drop(open_writer(&path).unwrap());
        let mut conn = open_writer(&path).unwrap();
        assert_eq!(migrations::migrate(&mut conn, &path).unwrap(), 0);
    }

    #[test]
    fn trigram_fts_matches_thai_substring() {
        let (_d, path) = temp_db();
        let conn = open_writer(&path).unwrap();
        conn.execute(
            "INSERT INTO memories_tri(rowid, title, body) VALUES (1, 'ระบบ', 'ระบบความจำสำหรับเอเจนต์เขียนโค้ด')",
            [],
        )
        .unwrap();
        let hit: i64 = conn
            .query_row("SELECT rowid FROM memories_tri WHERE memories_tri MATCH '\"ความจำ\"'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hit, 1);
    }

    #[test]
    fn readers_see_committed_writes_and_cannot_write() {
        let (_d, path) = temp_db();
        let writer = open_writer(&path).unwrap();
        writer.execute("INSERT INTO meta(key, value) VALUES ('k', 'v')", []).unwrap();
        let pool = ReaderPool::new(path.clone(), 2);
        let v: String = pool
            .with(|c| Ok(c.query_row("SELECT value FROM meta WHERE key='k'", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(v, "v");
        let err = pool.with(|c| Ok(c.execute("INSERT INTO meta(key, value) VALUES ('x','y')", [])?));
        assert!(err.is_err());
    }

    #[test]
    fn backup_into_produces_a_readable_copy() {
        let (d, path) = temp_db();
        let conn = open_writer(&path).unwrap();
        let dest = d.path().join("copy.db");
        backup_into(&conn, &dest).unwrap();
        let copy = rusqlite::Connection::open(&dest).unwrap();
        let n: i64 = copy.query_row("SELECT count(*) FROM projects", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }
}
```

- [ ] **Step 3: Run to confirm failure**

Run: `nice -n 15 cargo test -j 6 -p alice-core store::`
Expected: FAIL (functions missing).

- [ ] **Step 4: Implement `store/mod.rs`**

```rust
//! SQLite storage. Only the daemon opens the database.

pub mod migrations;

use crate::error::Result;
use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn configure_common(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_millis(5_000))?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA mmap_size = 268435456;
         PRAGMA cache_size = -65536;",
    )?;
    Ok(())
}

/// Open the single writer connection: creates the file, enables WAL and applies migrations.
pub fn open_writer(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut conn = Connection::open(path)?;
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.execute_batch("PRAGMA synchronous = NORMAL;")?;
    configure_common(&conn)?;
    migrations::migrate(&mut conn, path)?;
    Ok(conn)
}

/// Open a read-only connection for queries.
pub fn open_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI,
    )?;
    configure_common(&conn)?;
    Ok(conn)
}

/// Consistent snapshot of a live database.
pub fn backup_into(conn: &Connection, dest: &Path) -> Result<()> {
    if dest.exists() {
        std::fs::remove_file(dest)?;
    }
    conn.execute("VACUUM INTO ?1", [dest.to_string_lossy().as_ref()])?;
    Ok(())
}

/// Small pool of read-only connections. Connections are opened lazily and reused.
pub struct ReaderPool {
    path: PathBuf,
    max_idle: usize,
    idle: Mutex<Vec<Connection>>,
}

impl ReaderPool {
    pub fn new(path: PathBuf, max_idle: usize) -> Self {
        Self { path, max_idle, idle: Mutex::new(Vec::new()) }
    }

    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let pooled = self.idle.lock().pop();
        let conn = match pooled {
            Some(c) => c,
            None => open_reader(&self.path)?,
        };
        let out = f(&conn);
        let mut idle = self.idle.lock();
        if idle.len() < self.max_idle {
            idle.push(conn);
        }
        out
    }
}
```

- [ ] **Step 5: Implement `store/migrations.rs`**

```rust
//! Forward-only, numbered migrations. A backup is taken before migrating an existing database.

use crate::error::Result;
use crate::ids::now_rfc3339;
use rusqlite::{Connection, params};
use std::collections::HashSet;
use std::path::Path;

const MIGRATIONS: &[(i64, &str)] = &[(1, include_str!("sql/0001_init.sql"))];

pub const CURRENT_VERSION: i64 = 1;

pub fn migrate(conn: &mut Connection, db_path: &Path) -> Result<usize> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
    )?;
    let applied: HashSet<i64> = {
        let mut stmt = conn.prepare("SELECT version FROM schema_migrations")?;
        stmt.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
    };
    let todo: Vec<&(i64, &str)> = MIGRATIONS.iter().filter(|(v, _)| !applied.contains(v)).collect();
    if todo.is_empty() {
        return Ok(0);
    }
    if !applied.is_empty() {
        let stamp = now_rfc3339().replace([':', '.'], "-");
        let dest = db_path.with_file_name(format!("alice.pre-migration-{stamp}.db"));
        super::backup_into(conn, &dest)?;
    }
    for (version, sql) in &todo {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            params![version, now_rfc3339()],
        )?;
        tx.commit()?;
    }
    Ok(todo.len())
}
```

Add `pub mod store;` to `lib.rs`.

- [ ] **Step 6: Run tests**

Run: `nice -n 15 cargo test -j 6 -p alice-core store::`
Expected: PASS (5 tests).

- [ ] **Step 7: Commit**

```bash
git add crates/alice-core
git commit -m "feat(core): SQLite schema v1, migrations with pre-migration backup, reader pool

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: Project detection and project store

**Files:**
- Create: `crates/alice-core/src/project.rs`, `crates/alice-core/src/store/projects.rs`
- Modify: `crates/alice-core/src/lib.rs` (`pub mod project;`), `crates/alice-core/src/store/mod.rs` (`pub mod projects;`)
- Test: inside both files

**Interfaces:**
- Consumes: `normalize::{slugify, sha256_hex}`, `ids::{new_id, now_rfc3339}`
- Produces:
  - `project::ProjectRef { key: String, slug: String, name: String, root: Option<PathBuf>, remote: Option<String> }` with `ProjectRef::global()`
  - `project::normalize_remote(url: &str) -> Option<String>`
  - `project::detect_project(cwd: &Path) -> ProjectRef` (never fails; `global` fallback)
  - `store::projects::{ResolvedProject { id: String, slug: String, name: String }, GLOBAL_ID: &str, upsert_project(conn: &Connection, p: &ProjectRef) -> Result<ResolvedProject>, resolve_slug(conn: &Connection, slug_or_alias: &str) -> Result<Option<ResolvedProject>>, add_alias(conn: &Connection, alias: &str, project_id: &str) -> Result<()>, list_projects(conn: &Connection) -> Result<Vec<ResolvedProject>>, get_by_id(conn: &Connection, id: &str) -> Result<ResolvedProject>, find_by_key(conn: &Connection, key: &str) -> Result<Option<ResolvedProject>>}`

- [ ] **Step 1: Write failing tests for `project.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn remotes_normalise_to_host_and_path() {
        assert_eq!(normalize_remote("git@github.com:icueth/alice.git").as_deref(), Some("github.com/icueth/alice"));
        assert_eq!(normalize_remote("https://github.com/icueth/Alice.git/").as_deref(), Some("github.com/icueth/alice"));
        assert_eq!(normalize_remote("ssh://git@gitlab.example.com:2222/a/b.git").as_deref(), Some("gitlab.example.com/a/b"));
        assert_eq!(normalize_remote("https://user:secret@host.io/x/y").as_deref(), Some("host.io/x/y"));
        assert_eq!(normalize_remote(""), None);
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap().status.success();
        assert!(ok, "git {args:?} failed");
    }

    #[test]
    fn repo_with_remote_uses_remote_key_and_worktree_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("my_app");
        std::fs::create_dir(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["remote", "add", "origin", "git@github.com:me/My_App.git"]);
        git(&main, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "x"]);
        let p = detect_project(&main);
        assert_eq!(p.key, "github.com/me/my_app");
        assert_eq!(p.slug, "my-app");

        let wt = tmp.path().join("my_app-wt");
        git(&main, &["worktree", "add", "-q", "-b", "feature", wt.to_str().unwrap()]);
        assert_eq!(detect_project(&wt).key, p.key);
        assert_eq!(detect_project(&wt.join(".")).slug, "my-app");
    }

    #[test]
    fn repo_without_remote_uses_path_key() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("Local Tool");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        let p = detect_project(&repo.join("."));
        assert!(p.key.starts_with("path:"), "{}", p.key);
        assert_eq!(p.slug, "local-tool");
        assert!(p.remote.is_none());
    }

    #[test]
    fn plain_directory_home_and_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("notes");
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(detect_project(&dir).slug, "notes");
        if let Some(home) = std::env::home_dir() {
            assert_eq!(detect_project(&home).key, "global");
        }
        assert_eq!(detect_project(Path::new("/definitely/not/here/xyz")).key, "global");
        assert_eq!(detect_project(Path::new("/")).key, "global");
    }
}
```

- [ ] **Step 2: Implement `project.rs`**

```rust
//! Detect which project a working directory belongs to.

use crate::normalize::slugify;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRef {
    /// Stable identity: normalized remote, `path:<root>`, or `global`.
    pub key: String,
    pub slug: String,
    pub name: String,
    pub root: Option<PathBuf>,
    pub remote: Option<String>,
}

impl ProjectRef {
    pub fn global() -> Self {
        Self { key: "global".into(), slug: "global".into(), name: "Global".into(), root: None, remote: None }
    }
}

/// `git@github.com:Me/Repo.git` -> `github.com/me/repo`. Credentials and ports are dropped.
pub fn normalize_remote(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    if url.is_empty() {
        return None;
    }
    let (host, path) = if let Some(rest) = url.split_once("://").map(|(_, r)| r) {
        let rest = rest.rsplit_once('@').map(|(_, r)| r).unwrap_or(rest);
        let (host, path) = rest.split_once('/')?;
        (host.split(':').next()?.to_string(), path.to_string())
    } else if let Some((user_host, path)) = url.split_once(':') {
        let host = user_host.rsplit_once('@').map(|(_, h)| h).unwrap_or(user_host);
        (host.to_string(), path.to_string())
    } else {
        return None;
    };
    let path = path.trim_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{host}/{path}").to_lowercase())
}

fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn is_home_or_root(dir: &Path) -> bool {
    dir.parent().is_none() || std::env::home_dir().is_some_and(|h| h == dir)
}

/// Resolve the project for `cwd`. Never fails: unknown or unusable directories map to `global`.
pub fn detect_project(cwd: &Path) -> ProjectRef {
    let Ok(dir) = cwd.canonicalize() else { return ProjectRef::global() };
    if is_home_or_root(&dir) {
        return ProjectRef::global();
    }
    if let Some(root) = git_output(&dir, &["rev-parse", "--show-toplevel"]) {
        let root = PathBuf::from(root);
        let remote_raw = git_output(&root, &["config", "--get", "remote.origin.url"]);
        let remote = remote_raw.as_deref().and_then(normalize_remote);
        let name = match &remote {
            Some(r) => r.rsplit('/').next().unwrap_or(r).to_string(),
            None => root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        };
        let key = remote.clone().unwrap_or_else(|| format!("path:{}", root.display()));
        let slug = slugify(&name);
        if slug.is_empty() {
            return ProjectRef::global();
        }
        return ProjectRef { key, slug, name, root: Some(root), remote };
    }
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let slug = slugify(&name);
    if slug.is_empty() {
        return ProjectRef::global();
    }
    ProjectRef { key: format!("path:{}", dir.display()), slug, name, root: Some(dir), remote: None }
}
```

> If `std::env::home_dir` triggers a deprecation warning under clippy on 1.99, replace it with `dirs::home_dir()` (dirs 7.0.0) in both the code and the test.
>
> Worktrees: `git rev-parse --show-toplevel` returns the worktree directory, but `remote.origin.url` is shared, so the key matches the main checkout. A worktree of a repo **without** a remote gets its own `path:` key; that is acceptable and documented.

- [ ] **Step 3: Write failing tests for `store/projects.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectRef;
    use crate::store::open_writer;

    fn conn() -> (tempfile::TempDir, rusqlite::Connection) {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        (d, c)
    }

    fn pref(key: &str, slug: &str) -> ProjectRef {
        ProjectRef { key: key.into(), slug: slug.into(), name: slug.into(), root: None, remote: None }
    }

    #[test]
    fn upsert_is_stable_and_registers_slug_alias() {
        let (_d, c) = conn();
        let a = upsert_project(&c, &pref("github.com/me/app", "app")).unwrap();
        let b = upsert_project(&c, &pref("github.com/me/app", "app")).unwrap();
        assert_eq!(a, b);
        assert_eq!(resolve_slug(&c, "APP").unwrap().unwrap().id, a.id);
    }

    #[test]
    fn slug_collision_gets_a_suffix() {
        let (_d, c) = conn();
        let a = upsert_project(&c, &pref("github.com/me/api", "api")).unwrap();
        let b = upsert_project(&c, &pref("github.com/other/api", "api")).unwrap();
        assert_eq!(a.slug, "api");
        assert_ne!(b.slug, "api");
        assert!(b.slug.starts_with("api-"), "{}", b.slug);
    }

    #[test]
    fn aliases_fold_name_variants() {
        let (_d, c) = conn();
        let p = upsert_project(&c, &pref("github.com/me/pg-ai", "pg-ai")).unwrap();
        add_alias(&c, "wing_pg_ai", &p.id).unwrap();
        assert_eq!(resolve_slug(&c, "pg_ai").unwrap().unwrap().id, p.id);
        assert_eq!(resolve_slug(&c, "Wing-PG-AI").unwrap().unwrap().id, p.id);
        assert!(resolve_slug(&c, "nope").unwrap().is_none());
    }

    #[test]
    fn global_always_exists() {
        let (_d, c) = conn();
        assert_eq!(resolve_slug(&c, "global").unwrap().unwrap().id, GLOBAL_ID);
        assert_eq!(upsert_project(&c, &ProjectRef::global()).unwrap().id, GLOBAL_ID);
    }
}
```

- [ ] **Step 4: Implement `store/projects.rs`**

```rust
//! Projects and their aliases.

use crate::error::{AliceError, Result};
use crate::ids::{new_id, now_rfc3339};
use crate::normalize::{sha256_hex, slugify};
use crate::project::ProjectRef;
use rusqlite::{Connection, OptionalExtension, params};

pub const GLOBAL_ID: &str = "00000000000000000000GLOBAL";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ResolvedProject {
    pub id: String,
    pub slug: String,
    pub name: String,
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ResolvedProject> {
    Ok(ResolvedProject { id: r.get(0)?, slug: r.get(1)?, name: r.get(2)? })
}

pub fn get_by_id(conn: &Connection, id: &str) -> Result<ResolvedProject> {
    conn.query_row("SELECT id, slug, name FROM projects WHERE id = ?1", [id], row)
        .optional()?
        .ok_or_else(|| AliceError::NotFound(format!("project {id}")))
}

/// Insert the project if its key is new; returns the stored project either way.
pub fn upsert_project(conn: &Connection, p: &ProjectRef) -> Result<ResolvedProject> {
    if let Some(found) = conn
        .query_row("SELECT id, slug, name FROM projects WHERE key = ?1", [&p.key], row)
        .optional()?
    {
        return Ok(found);
    }
    let base = slugify(&p.slug);
    let taken = |s: &str| -> Result<bool> {
        let n: i64 = conn.query_row(
            "SELECT (SELECT count(*) FROM projects WHERE slug = ?1) + (SELECT count(*) FROM project_aliases WHERE alias = ?1)",
            [s],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    };
    let slug = if taken(&base)? { format!("{base}-{}", &sha256_hex(p.key.as_bytes())[..6]) } else { base };
    let id = new_id();
    conn.execute(
        "INSERT INTO projects (id, key, slug, name, git_remote, root_path, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            p.key,
            slug,
            p.name,
            p.remote,
            p.root.as_ref().map(|r| r.to_string_lossy().into_owned()),
            now_rfc3339()
        ],
    )?;
    Ok(ResolvedProject { id, slug, name: p.name.clone() })
}

/// Resolve a slug or any registered alias (case and separator insensitive).
pub fn resolve_slug(conn: &Connection, slug_or_alias: &str) -> Result<Option<ResolvedProject>> {
    let s = slugify(slug_or_alias);
    if let Some(p) = conn.query_row("SELECT id, slug, name FROM projects WHERE slug = ?1", [&s], row).optional()? {
        return Ok(Some(p));
    }
    Ok(conn
        .query_row(
            "SELECT p.id, p.slug, p.name FROM project_aliases a JOIN projects p ON p.id = a.project_id WHERE a.alias = ?1",
            [&s],
            row,
        )
        .optional()?)
}

pub fn add_alias(conn: &Connection, alias: &str, project_id: &str) -> Result<()> {
    let a = slugify(alias);
    if a.is_empty() {
        return Err(AliceError::invalid("alias is empty after normalisation; use letters or digits"));
    }
    conn.execute(
        "INSERT INTO project_aliases (alias, project_id) VALUES (?1, ?2) ON CONFLICT(alias) DO UPDATE SET project_id = excluded.project_id",
        params![a, project_id],
    )?;
    Ok(())
}

/// Look up a project by its stable key without creating it (read paths).
pub fn find_by_key(conn: &Connection, key: &str) -> Result<Option<ResolvedProject>> {
    Ok(conn.query_row("SELECT id, slug, name FROM projects WHERE key = ?1", [key], row).optional()?)
}

pub fn list_projects(conn: &Connection) -> Result<Vec<ResolvedProject>> {
    let mut stmt = conn.prepare("SELECT id, slug, name FROM projects ORDER BY slug")?;
    let rows = stmt.query_map([], row)?.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}
```

> `upsert_project` for `ProjectRef::global()` hits the existing `global` key row inserted by the migration. `pg_ai` resolves because `slugify("pg_ai") == "pg-ai"`, the stored slug.

- [ ] **Step 5: Run tests**

Run: `nice -n 15 cargo test -j 6 -p alice-core project`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/alice-core
git commit -m "feat(core): project detection from git (worktrees, no-remote, home) and project store with aliases

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: Memories, sources and FTS maintenance

**Files:**
- Create: `crates/alice-core/src/store/{fts.rs,memories.rs,sources.rs}`
- Modify: `crates/alice-core/src/store/mod.rs` (`pub mod fts; pub mod memories; pub mod sources;`)
- Test: inside `memories.rs` and `sources.rs`

**Interfaces:**
- Consumes: `normalize::*`, `codetokens::expand_code_tokens`, `ids::*`, `model::*`, `store::projects::ResolvedProject`
- Produces:
  - `store::fts::{index_memory(conn, seq: i64, title: &str, body: &str, topics: &[String]) -> Result<()>, unindex_memory(conn, seq: i64) -> Result<()>, index_chunk(conn, seq: i64, text: &str) -> Result<()>, unindex_chunk(conn, seq: i64) -> Result<()>}`
  - `store::memories::NewMemoryInput { content: String, kind: Option<Kind>, title: Option<String>, topics: Vec<String>, importance: Option<u8>, supersedes: Vec<String>, idempotency_key: Option<String>, author: Author, legacy_id: Option<String>, created_at: Option<String> }`
  - `store::memories::PreparedMemory { id, project_id, project_slug, kind, title, body, topics, importance, content_hash, author, idempotency_key, supersedes, created_at, hlc, legacy_id }` (all `pub`)
  - `store::memories::prepare(input: NewMemoryInput, project: &ResolvedProject, hlc: String) -> Result<PreparedMemory>`
  - `store::memories::InsertOutcome { New { id: String, seq: i64 }, Duplicate { id: String } }`
  - `store::memories::{insert(conn, m: &PreparedMemory) -> Result<InsertOutcome>, get_many(conn, ids: &[String]) -> Result<Vec<Memory>>, get_by_seqs(conn, seqs: &[i64]) -> Result<Vec<(i64, Memory)>>, seq_of(conn, id: &str) -> Result<Option<i64>>, apply_update(conn, id: &str, update: &MemoryUpdate, hlc: String) -> Result<Memory>, touch_access(conn, seqs: &[i64]) -> Result<()>, set_status_by_seq(conn, seq: i64, status: Status) -> Result<()>}`
  - `store::memories::MemoryUpdate { action: UpdateAction, title: Option<String>, body: Option<String>, kind: Option<Kind>, topics: Option<Vec<String>>, importance: Option<u8>, superseded_by: Option<String> }`, `UpdateAction { Edit, Supersede, Archive, Forget, Restore }` (serde snake_case, JsonSchema)
  - `store::sources::{SourceInput { source_type: SourceType, uri: String, agent: Option<String>, session_id: Option<String>, started_at: Option<String>, ended_at: Option<String>, legacy_ref: Option<String> }, ChunkInput { ord: i64, text: String, authored_at: Option<String>, legacy_id: Option<String> }, IngestOutcome { source_id: String, inserted: usize, skipped: usize }, ingest(conn, project: &ResolvedProject, src: &SourceInput, chunks: &[ChunkInput]) -> Result<IngestOutcome>, validate_chunks(chunks: &[ChunkInput]) -> Result<()>, ChunkRow { seq: i64, id: String, source_id: String, project_id: String, text: String, created_at: String, authored_at: Option<String> }, get_chunks_by_seqs(conn, seqs: &[i64]) -> Result<Vec<ChunkRow>>}`

- [ ] **Step 1: Write failing tests (`memories.rs`)**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, Kind, Status, limits};
    use crate::store::{open_writer, projects};
    use crate::project::ProjectRef;

    fn setup() -> (tempfile::TempDir, rusqlite::Connection, projects::ResolvedProject) {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(
            &c,
            &ProjectRef { key: "k".into(), slug: "app".into(), name: "app".into(), root: None, remote: None },
        )
        .unwrap();
        (d, c, p)
    }

    fn input(content: &str) -> NewMemoryInput {
        NewMemoryInput { content: content.into(), ..Default::default() }
    }

    #[test]
    fn insert_then_get_round_trips() {
        let (_d, c, p) = setup();
        let mut i = input("Use WAL mode\nbecause readers must not block the writer");
        i.kind = Some(Kind::Decision);
        i.topics = vec!["SQLite".into(), " storage ".into()];
        i.author = Author { agent: Some("claude-code".into()), ..Default::default() };
        let m = prepare(i, &p, "h".into()).unwrap();
        let InsertOutcome::New { id, .. } = insert(&c, &m).unwrap() else { panic!("expected new") };
        let got = &get_many(&c, &[id.clone()]).unwrap()[0];
        assert_eq!(got.title, "Use WAL mode");
        assert_eq!(got.kind, Kind::Decision);
        assert_eq!(got.status, Status::Pending);
        assert_eq!(got.topics, vec!["sqlite".to_string(), "storage".to_string()]);
        assert_eq!(got.project, "app");
        assert_eq!(got.author.agent.as_deref(), Some("claude-code"));
    }

    #[test]
    fn exact_duplicates_and_idempotency_keys_return_existing_id() {
        let (_d, c, p) = setup();
        let a = insert(&c, &prepare(input("same  text"), &p, "h".into()).unwrap()).unwrap();
        let b = insert(&c, &prepare(input("same text"), &p, "h".into()).unwrap()).unwrap();
        let InsertOutcome::New { id, .. } = a else { panic!() };
        assert_eq!(b, InsertOutcome::Duplicate { id: id.clone() });

        let mut k1 = input("first wording");
        k1.idempotency_key = Some("retry-1".into());
        let mut k2 = input("second wording");
        k2.idempotency_key = Some("retry-1".into());
        let InsertOutcome::New { id: kid, .. } = insert(&c, &prepare(k1, &p, "h".into()).unwrap()).unwrap() else { panic!() };
        assert_eq!(insert(&c, &prepare(k2, &p, "h".into()).unwrap()).unwrap(), InsertOutcome::Duplicate { id: kid });
    }

    #[test]
    fn oversized_and_invalid_inputs_are_rejected_with_the_limit() {
        let (_d, _c, p) = setup();
        let big = "x".repeat(limits::MAX_CONTENT_BYTES + 1);
        let err = prepare(input(&big), &p, "h".into()).unwrap_err().to_string();
        assert!(err.contains("65536"), "{err}");
        assert!(prepare(input("   "), &p, "h".into()).is_err());
        let mut bad = input("ok");
        bad.importance = Some(9);
        assert!(prepare(bad, &p, "h".into()).unwrap_err().to_string().contains("1 to 5"));
        let mut many = input("ok");
        many.topics = (0..25).map(|i| format!("t{i}")).collect();
        assert!(prepare(many, &p, "h".into()).unwrap_err().to_string().contains("20"));
    }

    #[test]
    fn supersede_on_insert_and_update_actions() {
        let (_d, c, p) = setup();
        let InsertOutcome::New { id: old, .. } = insert(&c, &prepare(input("port is 8080"), &p, "h".into()).unwrap()).unwrap() else { panic!() };
        let mut newer = input("port is 9090");
        newer.supersedes = vec![old.clone()];
        let InsertOutcome::New { id: new_id, .. } = insert(&c, &prepare(newer, &p, "h".into()).unwrap()).unwrap() else { panic!() };
        assert_eq!(get_many(&c, &[old.clone()]).unwrap()[0].status, Status::Superseded);
        let link: i64 = c
            .query_row("SELECT count(*) FROM links WHERE from_id=?1 AND to_id=?2 AND type='supersedes'", [&new_id, &old], |r| r.get(0))
            .unwrap();
        assert_eq!(link, 1);

        let edited = apply_update(
            &c,
            &new_id,
            &MemoryUpdate { action: UpdateAction::Edit, body: Some("port is 9091".into()), ..Default::default() },
            "h2".into(),
        )
        .unwrap();
        assert_eq!(edited.body, "port is 9091");
        let forgotten = apply_update(&c, &new_id, &MemoryUpdate { action: UpdateAction::Forget, ..Default::default() }, "h3".into()).unwrap();
        assert_eq!(forgotten.status, Status::Trashed);
        let restored = apply_update(&c, &new_id, &MemoryUpdate { action: UpdateAction::Restore, ..Default::default() }, "h4".into()).unwrap();
        assert_eq!(restored.status, Status::Active);
        assert!(apply_update(&c, "NOPE", &MemoryUpdate::default(), "h".into()).is_err());
    }

    #[test]
    fn fts_rows_follow_edits() {
        let (_d, c, p) = setup();
        let InsertOutcome::New { id, seq } = insert(&c, &prepare(input("HostLinkReconnect retries"), &p, "h".into()).unwrap()).unwrap() else { panic!() };
        let hits = |q: &str| -> i64 {
            c.query_row("SELECT count(*) FROM memories_fts WHERE memories_fts MATCH ?1", [q], |r| r.get(0)).unwrap()
        };
        assert_eq!(hits("\"reconnect\""), 1);
        apply_update(&c, &id, &MemoryUpdate { action: UpdateAction::Edit, body: Some("totally different".into()), title: Some("other".into()), ..Default::default() }, "h".into()).unwrap();
        assert_eq!(hits("\"reconnect\""), 0);
        assert_eq!(hits("\"different\""), 1);
        let _ = seq;
    }
}
```

`sources.rs` tests:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceType;
    use crate::project::ProjectRef;
    use crate::store::{open_writer, projects};

    fn setup() -> (tempfile::TempDir, rusqlite::Connection, projects::ResolvedProject) {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        (d, c, p)
    }

    fn src() -> SourceInput {
        SourceInput { source_type: SourceType::Session, uri: "file:///t/s1.jsonl".into(), ..Default::default() }
    }

    fn chunk(ord: i64, text: &str) -> ChunkInput {
        ChunkInput { ord, text: text.into(), ..Default::default() }
    }

    #[test]
    fn ingest_is_incremental_by_ord() {
        let (_d, c, p) = setup();
        let a = ingest(&c, &p, &src(), &[chunk(0, "hello"), chunk(1, "world")]).unwrap();
        assert_eq!((a.inserted, a.skipped), (2, 0));
        let b = ingest(&c, &p, &src(), &[chunk(1, "world"), chunk(2, "again")]).unwrap();
        assert_eq!((b.inserted, b.skipped), (1, 1));
        assert_eq!(a.source_id, b.source_id);
        let n: i64 = c.query_row("SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH '\"again\"'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn limits_are_enforced_before_writing() {
        let (_d, c, p) = setup();
        let too_many: Vec<ChunkInput> = (0..1_001).map(|i| chunk(i, "x")).collect();
        assert!(ingest(&c, &p, &src(), &too_many).unwrap_err().to_string().contains("1000"));
        let big = chunk(0, &"y".repeat(40_000));
        assert!(ingest(&c, &p, &src(), &[big]).unwrap_err().to_string().contains("32768"));
        let n: i64 = c.query_row("SELECT count(*) FROM sources", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "nothing partial is written");
    }
}
```

- [ ] **Step 2: Run to confirm failure**

Run: `nice -n 15 cargo test -j 6 -p alice-core store::memories store::sources`
Expected: FAIL (missing items).

- [ ] **Step 3: Implement `store/fts.rs`**

```rust
//! Maintenance of the contentless FTS5 tables (rowid = memories.seq / chunks.seq).

use crate::codetokens::expand_code_tokens;
use crate::error::Result;
use rusqlite::{Connection, params};

pub fn index_memory(conn: &Connection, seq: i64, title: &str, body: &str, topics: &[String]) -> Result<()> {
    let code = expand_code_tokens(&format!("{title}\n{body}"));
    conn.execute(
        "INSERT INTO memories_fts (rowid, title, body, topics, code_tokens) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![seq, title, body, topics.join(" "), code],
    )?;
    conn.execute("INSERT INTO memories_tri (rowid, title, body) VALUES (?1, ?2, ?3)", params![seq, title, body])?;
    Ok(())
}

pub fn unindex_memory(conn: &Connection, seq: i64) -> Result<()> {
    conn.execute("DELETE FROM memories_fts WHERE rowid = ?1", [seq])?;
    conn.execute("DELETE FROM memories_tri WHERE rowid = ?1", [seq])?;
    Ok(())
}

pub fn index_chunk(conn: &Connection, seq: i64, text: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO chunks_fts (rowid, text, code_tokens) VALUES (?1, ?2, ?3)",
        params![seq, text, expand_code_tokens(text)],
    )?;
    conn.execute("INSERT INTO chunks_tri (rowid, text) VALUES (?1, ?2)", params![seq, text])?;
    Ok(())
}

pub fn unindex_chunk(conn: &Connection, seq: i64) -> Result<()> {
    conn.execute("DELETE FROM chunks_fts WHERE rowid = ?1", [seq])?;
    conn.execute("DELETE FROM chunks_tri WHERE rowid = ?1", [seq])?;
    Ok(())
}
```

- [ ] **Step 4: Implement `store/memories.rs`**

```rust
//! Knowledge-tier memories.

use crate::error::{AliceError, Result};
use crate::ids::{new_id, now_rfc3339};
use crate::model::{Author, Kind, Memory, Status, limits};
use crate::normalize::{content_hash, derive_title, nfc, normalize_topic};
use crate::store::fts;
use crate::store::projects::ResolvedProject;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default)]
pub struct NewMemoryInput {
    pub content: String,
    pub kind: Option<Kind>,
    pub title: Option<String>,
    pub topics: Vec<String>,
    pub importance: Option<u8>,
    pub supersedes: Vec<String>,
    pub idempotency_key: Option<String>,
    pub author: Author,
    pub legacy_id: Option<String>,
    /// Import only: keep the original creation time.
    pub created_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PreparedMemory {
    pub id: String,
    pub project_id: String,
    pub project_slug: String,
    pub kind: Kind,
    pub title: String,
    pub body: String,
    pub topics: Vec<String>,
    pub importance: u8,
    pub content_hash: String,
    pub author: Author,
    pub idempotency_key: Option<String>,
    pub supersedes: Vec<String>,
    pub created_at: String,
    pub hlc: String,
    pub legacy_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InsertOutcome {
    New { id: String, seq: i64 },
    Duplicate { id: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpdateAction {
    #[default]
    Edit,
    Supersede,
    Archive,
    Forget,
    Restore,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct MemoryUpdate {
    pub action: UpdateAction,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub kind: Option<Kind>,
    #[serde(default)]
    pub topics: Option<Vec<String>>,
    #[serde(default)]
    pub importance: Option<u8>,
    /// For `supersede`: the id of the memory that replaces this one.
    #[serde(default)]
    pub superseded_by: Option<String>,
}

fn normalize_topics(raw: &[String]) -> Result<Vec<String>> {
    if raw.len() > limits::MAX_TOPICS {
        return Err(AliceError::invalid(format!(
            "too many topics ({}); use at most {} short tags",
            raw.len(),
            limits::MAX_TOPICS
        )));
    }
    let mut out: Vec<String> = raw.iter().filter_map(|t| normalize_topic(t)).collect();
    out.sort();
    out.dedup();
    Ok(out)
}

fn check_importance(i: Option<u8>) -> Result<u8> {
    match i.unwrap_or(3) {
        v @ 1..=5 => Ok(v),
        v => Err(AliceError::invalid(format!("importance {v} is out of range; use 1 to 5 (3 is normal)"))),
    }
}

/// Validate and normalise a new memory. Nothing is written.
pub fn prepare(input: NewMemoryInput, project: &ResolvedProject, hlc: String) -> Result<PreparedMemory> {
    let content = nfc(input.content.trim());
    if content.is_empty() {
        return Err(AliceError::invalid("content is empty; send the text to remember"));
    }
    if content.len() > limits::MAX_CONTENT_BYTES {
        return Err(AliceError::invalid(format!(
            "content is {} bytes; the limit is {} bytes. Save a shorter summary or split it into several memories",
            content.len(),
            limits::MAX_CONTENT_BYTES
        )));
    }
    let title = match input.title.map(|t| nfc(t.trim())).filter(|t| !t.is_empty()) {
        Some(t) if t.chars().count() > limits::MAX_TITLE_CHARS => {
            return Err(AliceError::invalid(format!(
                "title is longer than {} characters; shorten it",
                limits::MAX_TITLE_CHARS
            )));
        }
        Some(t) => t,
        None => derive_title(&content),
    };
    let kind = input.kind.unwrap_or(Kind::Note);
    let topics = normalize_topics(&input.topics)?;
    let importance = check_importance(input.importance)?;
    let hash = content_hash(&project.slug, kind, &title, &content);
    Ok(PreparedMemory {
        id: new_id(),
        project_id: project.id.clone(),
        project_slug: project.slug.clone(),
        kind,
        title,
        body: content,
        topics,
        importance,
        content_hash: hash,
        author: input.author,
        idempotency_key: input.idempotency_key.filter(|k| !k.is_empty()),
        supersedes: input.supersedes,
        created_at: input.created_at.unwrap_or_else(now_rfc3339),
        hlc,
        legacy_id: input.legacy_id,
    })
}

pub fn seq_of(conn: &Connection, id: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row("SELECT seq FROM memories WHERE id = ?1 OR legacy_id = ?1 ORDER BY seq LIMIT 1", [id], |r| r.get(0))
        .optional()?)
}

pub fn set_status_by_seq(conn: &Connection, seq: i64, status: Status) -> Result<()> {
    conn.execute(
        "UPDATE memories SET status = ?1, updated_at = ?2 WHERE seq = ?3",
        params![status.as_str(), now_rfc3339(), seq],
    )?;
    Ok(())
}

fn add_link(conn: &Connection, from: &str, to: &str, kind: &str, by: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO links (from_id, to_id, type, created_by, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![from, to, kind, by, now_rfc3339()],
    )?;
    Ok(())
}

/// Insert a prepared memory (status `pending`) with its topics and FTS rows.
/// Exact duplicates and repeated idempotency keys return the existing id.
pub fn insert(conn: &Connection, m: &PreparedMemory) -> Result<InsertOutcome> {
    if let Some(key) = &m.idempotency_key {
        if let Some(id) = conn
            .query_row("SELECT id FROM memories WHERE idempotency_key = ?1", [key], |r| r.get::<_, String>(0))
            .optional()?
        {
            return Ok(InsertOutcome::Duplicate { id });
        }
    }
    if let Some(id) = conn
        .query_row(
            "SELECT id FROM memories WHERE project_id = ?1 AND content_hash = ?2 AND status <> 'trashed'",
            params![m.project_id, m.content_hash],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(InsertOutcome::Duplicate { id });
    }
    let now = now_rfc3339();
    conn.execute(
        "INSERT INTO memories (id, project_id, kind, title, body, importance, status, content_hash,
            author_agent, author_model, author_session, idempotency_key, created_at, updated_at, hlc, legacy_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            m.id, m.project_id, m.kind.as_str(), m.title, m.body, m.importance, m.content_hash,
            m.author.agent, m.author.model, m.author.session, m.idempotency_key, m.created_at, now, m.hlc, m.legacy_id
        ],
    )?;
    let seq = conn.last_insert_rowid();
    for t in &m.topics {
        conn.execute("INSERT OR IGNORE INTO memory_topics (memory_id, topic) VALUES (?1, ?2)", params![m.id, t])?;
    }
    fts::index_memory(conn, seq, &m.title, &m.body, &m.topics)?;
    let by = m.author.agent.clone().unwrap_or_else(|| "user".into());
    for old in &m.supersedes {
        if let Some(old_seq) = seq_of(conn, old)? {
            let old_id: String = conn.query_row("SELECT id FROM memories WHERE seq = ?1", [old_seq], |r| r.get(0))?;
            set_status_by_seq(conn, old_seq, Status::Superseded)?;
            add_link(conn, &m.id, &old_id, "supersedes", &by)?;
        }
    }
    Ok(InsertOutcome::New { id: m.id.clone(), seq })
}

const SELECT_MEMORY: &str = "SELECT m.seq, m.id, p.slug, m.kind, m.title, m.body, m.importance, m.status,
    m.author_agent, m.author_model, m.author_session, m.created_at, m.updated_at, m.access_count, m.legacy_id
    FROM memories m JOIN projects p ON p.id = m.project_id";

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, Memory)> {
    let kind: String = r.get(3)?;
    let status: String = r.get(7)?;
    Ok((
        r.get(0)?,
        Memory {
            id: r.get(1)?,
            project: r.get(2)?,
            kind: Kind::parse(&kind).unwrap_or(Kind::Note),
            title: r.get(4)?,
            body: r.get(5)?,
            topics: Vec::new(),
            importance: r.get(6)?,
            status: Status::parse(&status).unwrap_or(Status::Active),
            author: Author { agent: r.get(8)?, model: r.get(9)?, session: r.get(10)? },
            created_at: r.get(11)?,
            updated_at: r.get(12)?,
            access_count: r.get(13)?,
            legacy_id: r.get(14)?,
        },
    ))
}

fn load_topics(conn: &Connection, m: &mut Memory) -> Result<()> {
    let mut stmt = conn.prepare_cached("SELECT topic FROM memory_topics WHERE memory_id = ?1 ORDER BY topic")?;
    m.topics = stmt.query_map([&m.id], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
    Ok(())
}

/// Fetch by id or legacy id, in the order requested. Unknown ids are skipped.
pub fn get_many(conn: &Connection, ids: &[String]) -> Result<Vec<Memory>> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let sql = format!("{SELECT_MEMORY} WHERE m.id = ?1 OR m.legacy_id = ?1 ORDER BY m.seq LIMIT 1");
        if let Some((_, mut m)) = conn.query_row(&sql, [id], read_row).optional()? {
            load_topics(conn, &mut m)?;
            out.push(m);
        }
    }
    Ok(out)
}

/// Fetch by internal sequence numbers (search results). Order follows the input.
pub fn get_by_seqs(conn: &Connection, seqs: &[i64]) -> Result<Vec<(i64, Memory)>> {
    if seqs.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; seqs.len()].join(",");
    let sql = format!("{SELECT_MEMORY} WHERE m.seq IN ({placeholders})");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows: Vec<(i64, Memory)> =
        stmt.query_map(params_from_iter(seqs.iter()), read_row)?.collect::<std::result::Result<_, _>>()?;
    for (_, m) in rows.iter_mut() {
        load_topics(conn, m)?;
    }
    let pos = |s: i64| seqs.iter().position(|x| *x == s).unwrap_or(usize::MAX);
    rows.sort_by_key(|(s, _)| pos(*s));
    Ok(rows)
}

pub fn touch_access(conn: &Connection, seqs: &[i64]) -> Result<()> {
    let now = now_rfc3339();
    let mut stmt = conn.prepare_cached(
        "UPDATE memories SET access_count = access_count + 1, last_accessed_at = ?1 WHERE seq = ?2",
    )?;
    for s in seqs {
        stmt.execute(params![now, s])?;
    }
    Ok(())
}

/// Apply an edit or status change and return the updated memory.
pub fn apply_update(conn: &Connection, id: &str, u: &MemoryUpdate, hlc: String) -> Result<Memory> {
    let seq = seq_of(conn, id)?.ok_or_else(|| {
        AliceError::NotFound(format!("memory {id}; check the id returned by recall or remember"))
    })?;
    let (_, current) = get_by_seqs(conn, &[seq])?
        .pop()
        .ok_or_else(|| AliceError::NotFound(format!("memory {id}")))?;
    let now = now_rfc3339();
    match u.action {
        UpdateAction::Edit => {
            let title = u.title.as_deref().map(|t| nfc(t.trim())).unwrap_or(current.title.clone());
            let body = u.body.as_deref().map(|b| nfc(b.trim())).unwrap_or(current.body.clone());
            if body.is_empty() {
                return Err(AliceError::invalid("body cannot be empty; use action \"forget\" to remove a memory"));
            }
            if body.len() > limits::MAX_CONTENT_BYTES {
                return Err(AliceError::invalid(format!("body exceeds {} bytes", limits::MAX_CONTENT_BYTES)));
            }
            let kind = u.kind.unwrap_or(current.kind);
            let importance = match u.importance {
                Some(i) => check_importance(Some(i))?,
                None => current.importance,
            };
            let text_changed = title != current.title || body != current.body;
            let hash = content_hash(&current.project, kind, &title, &body);
            conn.execute(
                "UPDATE memories SET title=?1, body=?2, kind=?3, importance=?4, content_hash=?5, updated_at=?6, hlc=?7,
                    embed_state = CASE WHEN ?8 THEN 'pending' ELSE embed_state END WHERE seq=?9",
                params![title, body, kind.as_str(), importance, hash, now, hlc, text_changed, seq],
            )?;
            let topics = match &u.topics {
                Some(t) => {
                    let t = normalize_topics(t)?;
                    conn.execute("DELETE FROM memory_topics WHERE memory_id = ?1", [&current.id])?;
                    for topic in &t {
                        conn.execute("INSERT INTO memory_topics (memory_id, topic) VALUES (?1, ?2)", params![current.id, topic])?;
                    }
                    t
                }
                None => current.topics.clone(),
            };
            fts::unindex_memory(conn, seq)?;
            fts::index_memory(conn, seq, &title, &body, &topics)?;
        }
        UpdateAction::Supersede => {
            let by = u.superseded_by.as_deref().ok_or_else(|| {
                AliceError::invalid("supersede needs superseded_by: the id of the memory that replaces this one")
            })?;
            let by_seq = seq_of(conn, by)?.ok_or_else(|| AliceError::NotFound(format!("memory {by}")))?;
            let by_id: String = conn.query_row("SELECT id FROM memories WHERE seq = ?1", [by_seq], |r| r.get(0))?;
            set_status_by_seq(conn, seq, Status::Superseded)?;
            add_link(conn, &by_id, &current.id, "supersedes", "user")?;
        }
        UpdateAction::Archive => set_status_by_seq(conn, seq, Status::Archived)?,
        UpdateAction::Forget => set_status_by_seq(conn, seq, Status::Trashed)?,
        UpdateAction::Restore => set_status_by_seq(conn, seq, Status::Active)?,
    }
    let (_, updated) = get_by_seqs(conn, &[seq])?.pop().ok_or_else(|| AliceError::NotFound(id.to_string()))?;
    Ok(updated)
}
```

- [ ] **Step 5: Implement `store/sources.rs`**

```rust
//! Raw tier: sources and their ordered chunks.

use crate::error::{AliceError, Result};
use crate::ids::{new_id, now_rfc3339};
use crate::model::{SourceType, limits};
use crate::normalize::{nfc, sha256_hex};
use crate::store::fts;
use crate::store::projects::ResolvedProject;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SourceInput {
    pub source_type: SourceType,
    /// Stable identifier, e.g. `file:///abs/path/session.jsonl`.
    pub uri: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub ended_at: Option<String>,
    #[serde(default)]
    pub legacy_ref: Option<String>,
}

impl Default for SourceInput {
    fn default() -> Self {
        Self {
            source_type: SourceType::Session,
            uri: String::new(),
            agent: None,
            session_id: None,
            started_at: None,
            ended_at: None,
            legacy_ref: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ChunkInput {
    pub ord: i64,
    pub text: String,
    #[serde(default)]
    pub authored_at: Option<String>,
    #[serde(default)]
    pub legacy_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IngestOutcome {
    pub source_id: String,
    pub inserted: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone)]
pub struct ChunkRow {
    pub seq: i64,
    pub id: String,
    pub source_id: String,
    pub project_id: String,
    pub text: String,
    pub created_at: String,
    pub authored_at: Option<String>,
}

pub fn validate_chunks(chunks: &[ChunkInput]) -> Result<()> {
    if chunks.len() > limits::MAX_CHUNKS_PER_REQUEST {
        return Err(AliceError::invalid(format!(
            "{} chunks in one request; send at most {} per request",
            chunks.len(),
            limits::MAX_CHUNKS_PER_REQUEST
        )));
    }
    if let Some(c) = chunks.iter().find(|c| c.text.len() > limits::MAX_CHUNK_BYTES) {
        return Err(AliceError::invalid(format!(
            "chunk {} is {} bytes; split chunks to at most {} bytes",
            c.ord,
            c.text.len(),
            limits::MAX_CHUNK_BYTES
        )));
    }
    Ok(())
}

/// Create the source if new (identified by type + uri) and insert chunks whose `ord` is not stored yet.
pub fn ingest(conn: &Connection, project: &ResolvedProject, src: &SourceInput, chunks: &[ChunkInput]) -> Result<IngestOutcome> {
    validate_chunks(chunks)?;
    if src.uri.trim().is_empty() {
        return Err(AliceError::invalid("source uri is empty; use a stable identifier such as the transcript path"));
    }
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM sources WHERE type = ?1 AND uri = ?2",
            params![src.source_type.as_str(), src.uri],
            |r| r.get(0),
        )
        .optional()?;
    let source_id = match existing {
        Some(id) => {
            conn.execute(
                "UPDATE sources SET ended_at = COALESCE(?1, ended_at) WHERE id = ?2",
                params![src.ended_at, id],
            )?;
            id
        }
        None => {
            let id = new_id();
            conn.execute(
                "INSERT INTO sources (id, project_id, type, agent, uri, session_id, started_at, ended_at, legacy_ref, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    id, project.id, src.source_type.as_str(), src.agent, src.uri, src.session_id,
                    src.started_at, src.ended_at, src.legacy_ref, now_rfc3339()
                ],
            )?;
            id
        }
    };
    let mut inserted = 0;
    let mut skipped = 0;
    let now = now_rfc3339();
    for c in chunks {
        let text = nfc(&c.text);
        let changed = conn.execute(
            "INSERT OR IGNORE INTO chunks (id, source_id, project_id, ord, text, content_hash, authored_at, created_at, legacy_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![new_id(), source_id, project.id, c.ord, text, sha256_hex(text.as_bytes()), c.authored_at, now, c.legacy_id],
        )?;
        if changed == 1 {
            fts::index_chunk(conn, conn.last_insert_rowid(), &text)?;
            inserted += 1;
        } else {
            skipped += 1;
        }
    }
    Ok(IngestOutcome { source_id, inserted, skipped })
}

pub fn get_chunks_by_seqs(conn: &Connection, seqs: &[i64]) -> Result<Vec<ChunkRow>> {
    if seqs.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; seqs.len()].join(",");
    let sql = format!(
        "SELECT seq, id, source_id, project_id, text, created_at, authored_at FROM chunks WHERE seq IN ({placeholders})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows: Vec<ChunkRow> = stmt
        .query_map(params_from_iter(seqs.iter()), |r| {
            Ok(ChunkRow {
                seq: r.get(0)?,
                id: r.get(1)?,
                source_id: r.get(2)?,
                project_id: r.get(3)?,
                text: r.get(4)?,
                created_at: r.get(5)?,
                authored_at: r.get(6)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    let pos = |s: i64| seqs.iter().position(|x| *x == s).unwrap_or(usize::MAX);
    rows.sort_by_key(|r| pos(r.seq));
    Ok(rows)
}
```

> The "nothing partial is written" test passes because `validate_chunks` runs before any insert. Callers (the writer thread) wrap `ingest` in a transaction, so a DB error mid-way also rolls back.

- [ ] **Step 6: Run tests**

Run: `nice -n 15 cargo test -j 6 -p alice-core store::`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/alice-core
git commit -m "feat(core): memory and raw-source storage with dedup, idempotency, limits and FTS maintenance

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: Knowledge graph store

**Files:**
- Create: `crates/alice-core/src/store/kg.rs`
- Modify: `crates/alice-core/src/store/mod.rs` (`pub mod kg;`)
- Test: inside `kg.rs`

**Interfaces:**
- Consumes: `ids::*`, `normalize::nfc`
- Produces:
  - `store::kg::{FactAction { Add, Supersede, End } (serde snake_case, JsonSchema, Default = Add), ObjectKind { Auto, Entity, Literal }, AssertFact { subject: String, predicate: String, object: String, object_kind: ObjectKind, valid_from: Option<String>, action: FactAction, source_memory_id: Option<String> }, FactOutcome { relation_id: Option<String>, ended: usize, predicate: String, suggestion: Option<String> }, Fact { id: String, subject: String, predicate: String, object: String, object_is_entity: bool, valid_from: String, valid_to: Option<String>, source_memory_id: Option<String> }, FactsQuery { entity: String, predicate: Option<String>, as_of: Option<String>, timeline: bool }}`
  - `store::kg::{alias_key(&str) -> String, normalize_predicate(&str) -> String, ensure_entity(conn, name: &str) -> Result<String>, find_entity(conn, name: &str) -> Result<Option<(String, String)>>, assert_fact(conn, f: &AssertFact, hlc: String) -> Result<FactOutcome>, query_facts(conn, q: &FactsQuery) -> Result<Vec<Fact>>, EntityMatcher::{load(conn) -> Result<EntityMatcher>, find_in(&self, text: &str) -> Vec<String /*entity id*/>}, link_mentions(conn, memory_id: &str, entity_ids: &[String]) -> Result<usize>}`

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::open_writer;

    fn conn() -> (tempfile::TempDir, rusqlite::Connection) {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        (d, c)
    }

    fn fact(s: &str, p: &str, o: &str, action: FactAction, from: &str) -> AssertFact {
        AssertFact {
            subject: s.into(),
            predicate: p.into(),
            object: o.into(),
            object_kind: ObjectKind::Auto,
            valid_from: Some(from.into()),
            action,
            source_memory_id: None,
        }
    }

    #[test]
    fn predicates_are_normalised() {
        assert_eq!(normalize_predicate("Has Bug"), "has_bug");
        assert_eq!(normalize_predicate("deploys-to"), "deploys_to");
    }

    #[test]
    fn add_is_idempotent_and_query_returns_current() {
        let (_d, c) = conn();
        let a = assert_fact(&c, &fact("Alice", "uses", "SQLite", FactAction::Add, "2026-10-01T00:00:00.000Z"), "h".into()).unwrap();
        let b = assert_fact(&c, &fact("alice", "Uses", "sqlite", FactAction::Add, "2026-10-02T00:00:00.000Z"), "h".into()).unwrap();
        assert_eq!(a.relation_id, b.relation_id);
        let facts = query_facts(&c, &FactsQuery { entity: "ALICE".into(), ..Default::default() }).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].object, "SQLite");
        assert!(facts[0].object_is_entity);
    }

    #[test]
    fn supersede_ends_previous_value_and_as_of_sees_history() {
        let (_d, c) = conn();
        assert_fact(&c, &fact("api", "runs_on", "port 8080", FactAction::Add, "2026-01-01T00:00:00.000Z"), "h".into()).unwrap();
        let out = assert_fact(&c, &fact("api", "runs_on", "port 9090", FactAction::Supersede, "2026-06-01T00:00:00.000Z"), "h".into()).unwrap();
        assert_eq!(out.ended, 1);
        let now = query_facts(&c, &FactsQuery { entity: "api".into(), ..Default::default() }).unwrap();
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].object, "port 9090");
        let then = query_facts(&c, &FactsQuery { entity: "api".into(), as_of: Some("2026-03-01T00:00:00.000Z".into()), ..Default::default() }).unwrap();
        assert_eq!(then[0].object, "port 8080");
        let all = query_facts(&c, &FactsQuery { entity: "api".into(), timeline: true, ..Default::default() }).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].valid_from < all[1].valid_from);
    }

    #[test]
    fn end_closes_a_fact() {
        let (_d, c) = conn();
        assert_fact(&c, &fact("svc", "has_bug", "leak", FactAction::Add, "2026-01-01T00:00:00.000Z"), "h".into()).unwrap();
        let out = assert_fact(&c, &fact("svc", "has_bug", "leak", FactAction::End, "2026-02-01T00:00:00.000Z"), "h".into()).unwrap();
        assert_eq!(out.ended, 1);
        assert!(query_facts(&c, &FactsQuery { entity: "svc".into(), ..Default::default() }).unwrap().is_empty());
    }

    #[test]
    fn long_or_numeric_objects_are_literals() {
        let (_d, c) = conn();
        assert_fact(&c, &fact("svc", "listens_on", "127.0.0.1:8765", FactAction::Add, "2026-01-01T00:00:00.000Z"), "h".into()).unwrap();
        let f = &query_facts(&c, &FactsQuery { entity: "svc".into(), ..Default::default() }).unwrap()[0];
        assert!(!f.object_is_entity);
        assert!(find_entity(&c, "127.0.0.1:8765").unwrap().is_none());
    }

    #[test]
    fn matcher_finds_entity_names_in_text() {
        let (_d, c) = conn();
        let id = ensure_entity(&c, "Reborn Kingdom").unwrap();
        ensure_entity(&c, "Go").unwrap();
        let m = EntityMatcher::load(&c).unwrap();
        let found = m.find_in("Working on reborn kingdom today; going home");
        assert_eq!(found, vec![id], "short names must match whole words only");
    }

    #[test]
    fn unknown_entity_returns_empty() {
        let (_d, c) = conn();
        assert!(query_facts(&c, &FactsQuery { entity: "ghost".into(), ..Default::default() }).unwrap().is_empty());
    }
}
```

- [ ] **Step 2: Run to confirm failure**

Run: `nice -n 15 cargo test -j 6 -p alice-core store::kg`
Expected: FAIL.

- [ ] **Step 3: Implement `store/kg.rs`**

```rust
//! Knowledge graph: entities, predicates, time-bounded relations and mentions.

use crate::error::{AliceError, Result};
use crate::ids::{new_id, now_rfc3339};
use crate::normalize::nfc;
use rusqlite::{Connection, OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FactAction {
    #[default]
    Add,
    Supersede,
    End,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    /// Entity for short names; literal for long text, numbers, dates, URLs and addresses.
    #[default]
    Auto,
    Entity,
    Literal,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct AssertFact {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    #[serde(default)]
    pub object_kind: ObjectKind,
    /// When the fact became true (or, for `end`, when it stopped). Defaults to now.
    #[serde(default)]
    pub valid_from: Option<String>,
    #[serde(default)]
    pub action: FactAction,
    #[serde(default)]
    pub source_memory_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FactOutcome {
    pub relation_id: Option<String>,
    pub ended: usize,
    pub predicate: String,
    /// An existing predicate that looks similar (filled by the Judge in M2).
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Fact {
    pub id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub object_is_entity: bool,
    pub valid_from: String,
    pub valid_to: Option<String>,
    pub source_memory_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FactsQuery {
    pub entity: String,
    #[serde(default)]
    pub predicate: Option<String>,
    /// Only facts valid at this time.
    #[serde(default)]
    pub as_of: Option<String>,
    /// All facts, past and present, ordered by start time.
    #[serde(default)]
    pub timeline: bool,
}

pub fn alias_key(name: &str) -> String {
    nfc(name).to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn normalize_predicate(p: &str) -> String {
    let mut out = String::new();
    for ch in nfc(p).to_lowercase().chars() {
        if ch.is_alphanumeric() {
            out.push(ch);
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    out.trim_end_matches('_').to_string()
}

pub fn find_entity(conn: &Connection, name: &str) -> Result<Option<(String, String)>> {
    Ok(conn
        .query_row(
            "SELECT e.id, e.name FROM entity_aliases a JOIN entities e ON e.id = a.entity_id WHERE a.alias = ?1",
            [alias_key(name)],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

pub fn ensure_entity(conn: &Connection, name: &str) -> Result<String> {
    let name = nfc(name.trim());
    if name.is_empty() {
        return Err(AliceError::invalid("entity name is empty"));
    }
    if let Some((id, _)) = find_entity(conn, &name)? {
        return Ok(id);
    }
    let id = new_id();
    conn.execute("INSERT INTO entities (id, name, created_at) VALUES (?1, ?2, ?3)", params![id, name, now_rfc3339()])?;
    conn.execute("INSERT INTO entity_aliases (alias, entity_id) VALUES (?1, ?2)", params![alias_key(&name), id])?;
    Ok(id)
}

fn looks_literal(s: &str) -> bool {
    s.chars().count() > 60
        || s.contains('\n')
        || s.contains("://")
        || s.split_whitespace().count() > 6
        || s.chars().filter(|c| c.is_ascii_digit()).count() * 2 > s.chars().count()
        || s.contains(|c: char| c == ':' || c == '/') && s.chars().any(|c| c.is_ascii_digit())
}

fn ensure_predicate(conn: &Connection, raw: &str) -> Result<String> {
    let p = normalize_predicate(raw);
    if p.is_empty() {
        return Err(AliceError::invalid("predicate is empty; use a verb phrase such as uses or deployed_on"));
    }
    let canonical: Option<Option<String>> =
        conn.query_row("SELECT canonical FROM predicates WHERE name = ?1", [&p], |r| r.get(0)).optional()?;
    match canonical {
        Some(Some(c)) => Ok(c),
        Some(None) => Ok(p),
        None => {
            conn.execute("INSERT INTO predicates (name) VALUES (?1)", [&p])?;
            Ok(p)
        }
    }
}

pub fn assert_fact(conn: &Connection, f: &AssertFact, hlc: String) -> Result<FactOutcome> {
    let subject = ensure_entity(conn, &f.subject)?;
    let predicate = ensure_predicate(conn, &f.predicate)?;
    let object = nfc(f.object.trim());
    if object.is_empty() {
        return Err(AliceError::invalid("object is empty"));
    }
    let as_entity = match f.object_kind {
        ObjectKind::Entity => true,
        ObjectKind::Literal => false,
        ObjectKind::Auto => find_entity(conn, &object)?.is_some() || !looks_literal(&object),
    };
    let (obj_entity, obj_literal) = if as_entity {
        (Some(ensure_entity(conn, &object)?), None)
    } else {
        (None, Some(object.clone()))
    };
    let at = f.valid_from.clone().unwrap_or_else(now_rfc3339);

    let same_active = "subject_id = ?1 AND predicate = ?2 AND valid_to IS NULL
        AND object_entity_id IS ?3 AND object_literal IS ?4";
    match f.action {
        FactAction::End => {
            let ended = conn.execute(
                &format!("UPDATE relations SET valid_to = ?5 WHERE {same_active}"),
                params![subject, predicate, obj_entity, obj_literal, at],
            )?;
            Ok(FactOutcome { relation_id: None, ended, predicate, suggestion: None })
        }
        FactAction::Add | FactAction::Supersede => {
            let mut ended = 0;
            if f.action == FactAction::Supersede {
                ended = conn.execute(
                    "UPDATE relations SET valid_to = ?3 WHERE subject_id = ?1 AND predicate = ?2 AND valid_to IS NULL",
                    params![subject, predicate, at],
                )?;
            } else if let Some(id) = conn
                .query_row(
                    &format!("SELECT id FROM relations WHERE {same_active}"),
                    params![subject, predicate, obj_entity, obj_literal],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
            {
                return Ok(FactOutcome { relation_id: Some(id), ended: 0, predicate, suggestion: None });
            }
            let id = new_id();
            conn.execute(
                "INSERT INTO relations (id, subject_id, predicate, object_entity_id, object_literal, valid_from, source_memory_id, created_at, hlc)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![id, subject, predicate, obj_entity, obj_literal, at, f.source_memory_id, now_rfc3339(), hlc],
            )?;
            Ok(FactOutcome { relation_id: Some(id), ended, predicate, suggestion: None })
        }
    }
}

pub fn query_facts(conn: &Connection, q: &FactsQuery) -> Result<Vec<Fact>> {
    let Some((entity_id, _)) = find_entity(conn, &q.entity)? else { return Ok(Vec::new()) };
    let predicate = q.predicate.as_deref().map(normalize_predicate);
    let mut sql = String::from(
        "SELECT r.id, s.name, r.predicate, COALESCE(o.name, r.object_literal), r.object_entity_id IS NOT NULL,
                r.valid_from, r.valid_to, r.source_memory_id
         FROM relations r JOIN entities s ON s.id = r.subject_id LEFT JOIN entities o ON o.id = r.object_entity_id
         WHERE (r.subject_id = ?1 OR r.object_entity_id = ?1) AND (?2 IS NULL OR r.predicate = ?2)",
    );
    if !q.timeline {
        match &q.as_of {
            Some(_) => sql.push_str(" AND r.valid_from <= ?3 AND (r.valid_to IS NULL OR r.valid_to > ?3)"),
            None => sql.push_str(" AND r.valid_to IS NULL AND ?3 IS NULL"),
        }
    } else {
        sql.push_str(" AND (?3 IS NULL OR ?3 IS NOT NULL)");
    }
    sql.push_str(" ORDER BY r.valid_from, r.id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![entity_id, predicate, q.as_of], |r| {
            Ok(Fact {
                id: r.get(0)?,
                subject: r.get(1)?,
                predicate: r.get(2)?,
                object: r.get(3)?,
                object_is_entity: r.get(4)?,
                valid_from: r.get(5)?,
                valid_to: r.get(6)?,
                source_memory_id: r.get(7)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// In-memory list of entity aliases used to find mentions in memory text.
pub struct EntityMatcher {
    aliases: Vec<(String, String)>, // (alias_key, entity_id)
}

impl EntityMatcher {
    pub fn load(conn: &Connection) -> Result<Self> {
        let mut stmt = conn.prepare("SELECT alias, entity_id FROM entity_aliases")?;
        let aliases = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?;
        Ok(Self { aliases })
    }

    /// Entity ids whose alias occurs in `text`. ASCII aliases must match whole words.
    pub fn find_in(&self, text: &str) -> Vec<String> {
        let hay = alias_key(text);
        let mut out = Vec::new();
        for (alias, id) in &self.aliases {
            if alias.chars().count() < 2 {
                continue;
            }
            let found = if alias.is_ascii() {
                hay.match_indices(alias.as_str()).any(|(i, _)| {
                    let before = hay[..i].chars().next_back();
                    let after = hay[i + alias.len()..].chars().next();
                    !before.is_some_and(|c| c.is_alphanumeric()) && !after.is_some_and(|c| c.is_alphanumeric())
                })
            } else {
                hay.contains(alias.as_str())
            };
            if found && !out.contains(id) {
                out.push(id.clone());
            }
        }
        out
    }
}

pub fn link_mentions(conn: &Connection, memory_id: &str, entity_ids: &[String]) -> Result<usize> {
    let mut n = 0;
    for e in entity_ids {
        n += conn.execute(
            "INSERT OR IGNORE INTO mentions (memory_id, entity_id) VALUES (?1, ?2)",
            params![memory_id, e],
        )?;
    }
    Ok(n)
}
```

> The timeline branch must still bind `?3`; the parenthesised expression `(?3 IS NULL OR ?3 IS NOT NULL)` is always true and keeps one parameter shape. Keep the parentheses: without them `OR` would escape the entity filter.

- [ ] **Step 4: Run tests**

Run: `nice -n 15 cargo test -j 6 -p alice-core store::kg`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/alice-core
git commit -m "feat(core): knowledge graph with time-bounded facts, predicate normalisation and mention matching

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 6: Embedder trait, fake embedder, embedding storage, fastembed provider

**Files:**
- Create: `crates/alice-core/src/embed.rs`, `crates/alice-core/src/store/embeddings.rs`
- Create: `crates/alice-embed/Cargo.toml`, `crates/alice-embed/src/lib.rs`
- Modify: `Cargo.toml` (add `crates/alice-embed` to members; add `half`, `fastembed` to workspace deps), `crates/alice-core/Cargo.toml` (add `half`), `crates/alice-core/src/lib.rs` (`pub mod embed;`), `crates/alice-core/src/store/mod.rs` (`pub mod embeddings;`)
- Test: inside `embed.rs`, `store/embeddings.rs`; `crates/alice-embed/tests/real_model.rs` (ignored by default)

**Interfaces:**
- Consumes: `normalize::truncate_chars`, `store::fts` row ids (`memories.seq`, `chunks.seq`)
- Produces:
  - `alice_core::embed::Embedder` trait: `fn model_id(&self) -> &str; fn dim(&self) -> usize; fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>; fn embed_query(&self, text: &str) -> Result<Vec<f32>>;` (object safe, `Send + Sync`)
  - `alice_core::embed::FakeEmbedder::new(dim: usize) -> FakeEmbedder` (deterministic, similar text => similar vector)
  - `alice_core::embed::{memory_embed_text(title: &str, body: &str) -> String, cosine(a: &[f32], b: &[f32]) -> f32}`
  - `alice_core::store::embeddings::{VecKey { Memory(i64), Chunk(i64) }, encode_f16(&[f32]) -> Vec<u8>, decode_f16(&[u8]) -> Vec<f32>, PendingItem { key: VecKey, text: String }, pending(conn, limit: usize) -> Result<Vec<PendingItem>>, put(conn, key: VecKey, model: &str, v: &[f32]) -> Result<()>, mark_done(conn, key: VecKey) -> Result<()>, mark_failed(conn, key: VecKey, retry: bool) -> Result<()>, count(conn, model: &str) -> Result<usize>, for_each(conn, model: &str, f: impl FnMut(VecKey, Vec<f32>)) -> Result<()>, backlog(conn) -> Result<usize>}`
  - `alice_embed::{FastEmbedder::new(models_dir: &Path) -> alice_core::Result<FastEmbedder>, MODEL_ID: &str = "multilingual-e5-small-q8", DIM: usize = 384, model_dir(models_dir: &Path) -> PathBuf, is_model_present(models_dir: &Path) -> bool, async fetch_model(models_dir: &Path, progress: &(dyn Fn(&str, u64, u64) + Sync)) -> alice_core::Result<()>, install_tls_provider()}`

- [ ] **Step 1: Failing tests for `embed.rs` and `store/embeddings.rs`**

`embed.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_embedder_is_deterministic_normalised_and_similarity_aware() {
        let e = FakeEmbedder::new(384);
        let a = e.embed_query("sqlite writer lease timeout").unwrap();
        let b = e.embed_passages(&["sqlite writer lease timeout".to_string()]).unwrap().remove(0);
        let c = e.embed_query("sqlite writer lease timed out").unwrap();
        let d = e.embed_query("ภาษาไทย ความจำ").unwrap();
        assert_eq!(a.len(), 384);
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-5);
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-5, "query and passage of same text match");
        assert!(cosine(&a, &c) > 0.6, "near text is near: {}", cosine(&a, &c));
        assert!(cosine(&a, &d) < 0.3, "unrelated text is far: {}", cosine(&a, &d));
    }

    #[test]
    fn embed_text_is_bounded() {
        let t = memory_embed_text("title", &"x".repeat(10_000));
        assert!(t.chars().count() <= 2_001);
        assert!(t.starts_with("title\n"));
    }
}
```

`store/embeddings.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectRef;
    use crate::store::{memories, open_writer, projects, sources};

    #[test]
    fn f16_round_trip_is_close() {
        let v = vec![0.1_f32, -0.5, 0.25, 1.0];
        let back = decode_f16(&encode_f16(&v));
        for (a, b) in v.iter().zip(&back) {
            assert!((a - b).abs() < 1e-3);
        }
    }

    #[test]
    fn pending_lists_memories_then_chunks_and_mark_done_removes_them() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        let m = memories::prepare(memories::NewMemoryInput { content: "alpha".into(), ..Default::default() }, &p, "h".into()).unwrap();
        let memories::InsertOutcome::New { seq, .. } = memories::insert(&c, &m).unwrap() else { panic!() };
        sources::ingest(&c, &p, &sources::SourceInput { uri: "u".into(), ..Default::default() },
            &[sources::ChunkInput { ord: 0, text: "beta".into(), ..Default::default() }]).unwrap();
        let items = pending(&c, 10).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].key, VecKey::Memory(seq));
        assert!(items[0].text.contains("alpha"));
        assert!(matches!(items[1].key, VecKey::Chunk(_)));
        put(&c, items[0].key, "m", &[0.0, 1.0]).unwrap();
        mark_done(&c, items[0].key).unwrap();
        assert_eq!(pending(&c, 10).unwrap().len(), 1);
        assert_eq!(count(&c, "m").unwrap(), 1);
        assert_eq!(backlog(&c).unwrap(), 1);
        let mut seen = Vec::new();
        for_each(&c, "m", |k, v| seen.push((k, v))).unwrap();
        assert_eq!(seen[0].0, VecKey::Memory(seq));
    }

    #[test]
    fn failed_items_retry_then_stop() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        let m = memories::prepare(memories::NewMemoryInput { content: "gamma".into(), ..Default::default() }, &p, "h".into()).unwrap();
        let memories::InsertOutcome::New { seq, .. } = memories::insert(&c, &m).unwrap() else { panic!() };
        mark_failed(&c, VecKey::Memory(seq), true).unwrap();
        assert_eq!(pending(&c, 10).unwrap().len(), 1, "retry stays queued");
        mark_failed(&c, VecKey::Memory(seq), false).unwrap();
        assert!(pending(&c, 10).unwrap().is_empty(), "failed is not retried");
    }
}
```

- [ ] **Step 2: Run to confirm failure**

Run: `nice -n 15 cargo test -j 6 -p alice-core embed`
Expected: FAIL.

- [ ] **Step 3: Implement `crates/alice-core/src/embed.rs`**

```rust
//! Embedding provider interface and a deterministic fake for tests.

use crate::error::Result;
use crate::normalize::{nfc, truncate_chars};

pub trait Embedder: Send + Sync {
    /// Identifier stored with each vector, e.g. `multilingual-e5-small`.
    fn model_id(&self) -> &str;
    fn dim(&self) -> usize;
    fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn embed_query(&self, text: &str) -> Result<Vec<f32>>;
}

/// Text embedded for a memory: title and body, bounded so the model's 512-token window is not exceeded badly.
pub fn memory_embed_text(title: &str, body: &str) -> String {
    truncate_chars(&format!("{title}\n{body}"), 2_000)
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
}

/// Hashes words and character trigrams into a fixed-size, L2-normalised vector.
/// Texts that share many trigrams get high cosine similarity. No model download.
pub struct FakeEmbedder {
    dim: usize,
}

impl FakeEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }

    fn vector(&self, text: &str) -> Vec<f32> {
        fn fnv1a(bytes: &[u8]) -> u64 {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in bytes {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            h
        }
        let mut v = vec![0f32; self.dim];
        let text = nfc(text).to_lowercase();
        let mut add = |feature: &str, weight: f32| {
            let h = fnv1a(feature.as_bytes());
            let idx = (h % self.dim as u64) as usize;
            let sign = if (h >> 63) == 0 { 1.0 } else { -1.0 };
            v[idx] += sign * weight;
        };
        for word in text.split_whitespace() {
            add(word, 1.0);
        }
        let chars: Vec<char> = format!(" {text} ").chars().collect();
        for w in chars.windows(3) {
            add(&w.iter().collect::<String>(), 0.5);
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|x| *x /= norm);
        }
        v
    }
}

impl Embedder for FakeEmbedder {
    fn model_id(&self) -> &str {
        "fake"
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.vector(t)).collect())
    }
    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self.vector(text))
    }
}
```

- [ ] **Step 4: Implement `store/embeddings.rs`**

```rust
//! Vector storage (source of truth for the HNSW index) and the embedding work queue.

use crate::embed::memory_embed_text;
use crate::error::Result;
use half::f16;
use rusqlite::{Connection, params};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VecKey {
    Memory(i64),
    Chunk(i64),
}

impl VecKey {
    fn parts(self) -> (&'static str, i64) {
        match self {
            VecKey::Memory(s) => ("memory", s),
            VecKey::Chunk(s) => ("chunk", s),
        }
    }
}

pub fn encode_f16(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| f16::from_f32(*x).to_le_bytes()).collect()
}

pub fn decode_f16(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(2).map(|c| f16::from_le_bytes([c[0], c[1]]).to_f32()).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingItem {
    pub key: VecKey,
    pub text: String,
}

/// Up to `limit` items waiting for embeddings: memories first (they are what agents search), then chunks.
pub fn pending(conn: &Connection, limit: usize) -> Result<Vec<PendingItem>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare_cached(
        "SELECT seq, title, body FROM memories WHERE embed_state IN ('pending','retry') AND status <> 'trashed' ORDER BY seq LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |r| {
        Ok(PendingItem { key: VecKey::Memory(r.get(0)?), text: memory_embed_text(&r.get::<_, String>(1)?, &r.get::<_, String>(2)?) })
    })?;
    for r in rows {
        out.push(r?);
    }
    if out.len() < limit {
        let mut stmt = conn.prepare_cached(
            "SELECT seq, text FROM chunks WHERE embed_state IN ('pending','retry') ORDER BY seq LIMIT ?1",
        )?;
        let rows = stmt.query_map([(limit - out.len()) as i64], |r| {
            Ok(PendingItem { key: VecKey::Chunk(r.get(0)?), text: r.get(1)? })
        })?;
        for r in rows {
            out.push(r?);
        }
    }
    Ok(out)
}

pub fn put(conn: &Connection, key: VecKey, model: &str, v: &[f32]) -> Result<()> {
    let (t, s) = key.parts();
    conn.execute(
        "INSERT INTO embeddings (owner_type, owner_seq, model, dim, vector) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(owner_type, owner_seq, model) DO UPDATE SET dim = excluded.dim, vector = excluded.vector",
        params![t, s, model, v.len() as i64, encode_f16(v)],
    )?;
    Ok(())
}

fn set_state(conn: &Connection, key: VecKey, state: &str) -> Result<()> {
    match key {
        VecKey::Memory(s) => conn.execute("UPDATE memories SET embed_state = ?1 WHERE seq = ?2", params![state, s])?,
        VecKey::Chunk(s) => conn.execute("UPDATE chunks SET embed_state = ?1 WHERE seq = ?2", params![state, s])?,
    };
    Ok(())
}

pub fn mark_done(conn: &Connection, key: VecKey) -> Result<()> {
    set_state(conn, key, "done")
}

pub fn mark_failed(conn: &Connection, key: VecKey, retry: bool) -> Result<()> {
    set_state(conn, key, if retry { "retry" } else { "failed" })
}

pub fn count(conn: &Connection, model: &str) -> Result<usize> {
    let n: i64 = conn.query_row("SELECT count(*) FROM embeddings WHERE model = ?1", [model], |r| r.get(0))?;
    Ok(n as usize)
}

pub fn backlog(conn: &Connection) -> Result<usize> {
    let n: i64 = conn.query_row(
        "SELECT (SELECT count(*) FROM memories WHERE embed_state IN ('pending','retry') AND status <> 'trashed')
              + (SELECT count(*) FROM chunks WHERE embed_state IN ('pending','retry'))",
        [],
        |r| r.get(0),
    )?;
    Ok(n as usize)
}

pub fn for_each(conn: &Connection, model: &str, mut f: impl FnMut(VecKey, Vec<f32>)) -> Result<()> {
    let mut stmt = conn.prepare("SELECT owner_type, owner_seq, vector FROM embeddings WHERE model = ?1")?;
    let mut rows = stmt.query([model])?;
    while let Some(r) = rows.next()? {
        let t: String = r.get(0)?;
        let s: i64 = r.get(1)?;
        let blob: Vec<u8> = r.get(2)?;
        let key = if t == "memory" { VecKey::Memory(s) } else { VecKey::Chunk(s) };
        f(key, decode_f16(&blob));
    }
    Ok(())
}
```

Add `half = "2.7.1"` to `[workspace.dependencies]` and `half.workspace = true` to `alice-core`.

- [ ] **Step 5: Run core tests**

Run: `nice -n 15 cargo test -j 6 -p alice-core embed`
Expected: PASS.

- [ ] **Step 6: Create `alice-embed` with the fastembed provider**

Workspace additions (verified together in the spike):
```toml
tokio = { version = "1.53.2", features = ["rt-multi-thread", "macros", "sync", "time", "signal", "net", "fs", "io-util"] }
fastembed = { version = "7.1.1", default-features = false, features = ["ort-download-binaries-rustls-tls"] }
reqwest = { version = "0.13.5", default-features = false, features = ["rustls-no-provider", "http2", "stream", "json"] }
rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }
futures = "0.3.34"
```

> If `fastembed` does not compile without its Hugging Face downloader, add the `hf-hub-rustls-tls` feature; Alice still downloads the model itself (below), so the downloader stays unused.
>
> ONNX Runtime is linked statically by `ort-download-binaries`; nothing ships next to the binary. Prebuilt ORT exists for aarch64-apple-darwin, x86_64/aarch64-unknown-linux-gnu and x86_64/aarch64-pc-windows-msvc only (no Intel macOS, no musl). Those targets build `alice-embed` without the `local` feature and use an HTTP embedding provider (M2).

`crates/alice-embed/Cargo.toml`:
```toml
[package]
name = "alice-embed"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
rust-version.workspace = true
description = "Embedding providers for Alice"

[features]
default = ["local"]
local = ["dep:fastembed"]

[dependencies]
alice-core = { path = "../alice-core" }
fastembed = { workspace = true, optional = true }
reqwest.workspace = true
rustls.workspace = true
futures.workspace = true
sha2.workspace = true
hex.workspace = true
parking_lot.workspace = true
tokio.workspace = true
tracing.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

`crates/alice-embed/src/lib.rs`:
```rust
//! Embedding providers. The local provider runs multilingual-e5-small (int8, 118 MB) through ONNX Runtime.
//! Model files are pinned to one Hugging Face revision and verified by SHA-256.

use alice_core::error::{AliceError, Result};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const MODEL_ID: &str = "multilingual-e5-small-q8";
pub const DIM: usize = 384;
const REPO: &str = "Xenova/multilingual-e5-small";
const REVISION: &str = "761b726dd34fb83930e26aab4e9ac3899aa1fa78";

/// (path in the repo, local file name, size in bytes, sha256)
const FILES: &[(&str, &str, u64, &str)] = &[
    ("onnx/model_quantized.onnx", "model_quantized.onnx", 118_308_185, "f80102d3f2a1229f387d3c81909990d8945513e347b0eab049f7de3c6f98c193"),
    ("tokenizer.json", "tokenizer.json", 17_082_730, "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39"),
    ("config.json", "config.json", 658, "cb99455288675345e1a4f411438d5d0adbba5fbd3a67ea4fb03c015433b996c1"),
    ("special_tokens_map.json", "special_tokens_map.json", 167, "d05497f1da52c5e09554c0cd874037a083e1dc1b9cfd48034d1c717f1afc07a7"),
    ("tokenizer_config.json", "tokenizer_config.json", 443, "a1d6bc8734a6f635dc158508bef000f8e2e5a759c7d92f984b2c86e5ff53425b"),
];

/// reqwest is built with `rustls-no-provider`; every binary must install a crypto provider once,
/// before any HTTP client (including rmcp's) is created. Safe to call repeatedly.
pub fn install_tls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn model_dir(models_dir: &Path) -> PathBuf {
    models_dir.join(MODEL_ID)
}

pub fn is_model_present(models_dir: &Path) -> bool {
    let dir = model_dir(models_dir);
    FILES.iter().all(|(_, name, size, _)| std::fs::metadata(dir.join(name)).is_ok_and(|m| m.len() == *size))
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(hex::encode(h.finalize()))
}

/// Download missing or damaged model files. `progress(file, bytes_done, bytes_total)` is called while downloading.
pub async fn fetch_model(models_dir: &Path, progress: &(dyn Fn(&str, u64, u64) + Sync)) -> Result<()> {
    install_tls_provider();
    let dir = model_dir(models_dir);
    std::fs::create_dir_all(&dir)?;
    let client = reqwest::Client::builder()
        .user_agent(concat!("alice/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| AliceError::Other(format!("http client: {e}")))?;
    for (remote, name, size, sha) in FILES {
        let dest = dir.join(name);
        if dest.exists() && sha256_file(&dest)? == *sha {
            continue;
        }
        let url = format!("https://huggingface.co/{REPO}/resolve/{REVISION}/{remote}");
        let resp = client
            .get(&url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| AliceError::Other(format!("downloading {name} failed: {e}. Check the network or set HF_ENDPOINT to a mirror")))?;
        let part = dir.join(format!("{name}.part"));
        let mut file = std::fs::File::create(&part)?;
        let mut hasher = Sha256::new();
        let mut done = 0u64;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| AliceError::Other(format!("downloading {name} failed: {e}")))?;
            hasher.update(&chunk);
            std::io::Write::write_all(&mut file, &chunk)?;
            done += chunk.len() as u64;
            progress(name, done, *size);
        }
        drop(file);
        let got = hex::encode(hasher.finalize());
        if got != *sha {
            let _ = std::fs::remove_file(&part);
            return Err(AliceError::Other(format!("checksum mismatch for {name}: expected {sha}, got {got}")));
        }
        std::fs::rename(&part, &dest)?;
    }
    Ok(())
}

#[cfg(feature = "local")]
mod local {
    use super::*;
    use alice_core::embed::Embedder;
    use fastembed::{InitOptionsUserDefined, Pooling, QuantizationMode, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel};
    use parking_lot::Mutex;

    /// `TextEmbedding::embed` needs `&mut self`, so calls are serialised by a mutex.
    /// The daemon creates two instances (worker and queries) so they never wait on each other.
    pub struct FastEmbedder {
        model: Mutex<TextEmbedding>,
    }

    impl FastEmbedder {
        pub fn new(models_dir: &Path) -> Result<Self> {
            if !is_model_present(models_dir) {
                return Err(AliceError::Other("the embedding model is not downloaded; run \"alice model fetch\"".into()));
            }
            let dir = model_dir(models_dir);
            let rd = |f: &str| std::fs::read(dir.join(f));
            let model = UserDefinedEmbeddingModel::new(
                rd("model_quantized.onnx")?,
                TokenizerFiles {
                    tokenizer_file: rd("tokenizer.json")?,
                    config_file: rd("config.json")?,
                    special_tokens_map_file: rd("special_tokens_map.json")?,
                    tokenizer_config_file: rd("tokenizer_config.json")?,
                },
            )
            .with_pooling(Pooling::Mean)
            // Static: dynamic quantization forbids batching.
            .with_quantization(QuantizationMode::Static);
            let te = TextEmbedding::try_new_from_user_defined(model, InitOptionsUserDefined::new().with_max_length(512))
                .map_err(|e| AliceError::Other(format!("loading the embedding model failed: {e}")))?;
            Ok(Self { model: Mutex::new(te) })
        }
    }

    impl Embedder for FastEmbedder {
        fn model_id(&self) -> &str {
            MODEL_ID
        }
        fn dim(&self) -> usize {
            DIM
        }
        fn embed_passages(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            let inputs: Vec<String> = texts.iter().map(|t| format!("passage: {t}")).collect();
            self.model.lock().embed(inputs, Some(32)).map_err(|e| AliceError::Other(format!("embedding failed: {e}")))
        }
        fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
            let mut v = self
                .model
                .lock()
                .embed(vec![format!("query: {text}")], None)
                .map_err(|e| AliceError::Other(format!("embedding failed: {e}")))?;
            v.pop().ok_or_else(|| AliceError::Other("embedding returned nothing".into()))
        }
    }
}

#[cfg(feature = "local")]
pub use local::FastEmbedder;
```

`crates/alice-embed/tests/real_model.rs` (downloads 136 MB once; run on demand):
```rust
//! Run with: ALICE_MODELS_DIR=~/.alice/models cargo test -p alice-embed --release -- --ignored

use alice_core::embed::{Embedder, cosine};
use alice_embed::{FastEmbedder, fetch_model};
use std::path::PathBuf;

#[tokio::test]
#[ignore = "downloads the real model"]
async fn real_model_ranks_the_thai_match_first() {
    alice_embed::install_tls_provider();
    let dir = std::env::var_os("ALICE_MODELS_DIR").map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("alice-models"));
    fetch_model(&dir, &|_, _, _| {}).await.unwrap();
    let e = FastEmbedder::new(&dir).unwrap();
    let passages = vec![
        "ระบบความจำสำหรับเอเจนต์เขียนโค้ด".to_string(),
        "The deploy target is Easypanel".to_string(),
        "สูตรต้มยำกุ้ง".to_string(),
    ];
    let p = e.embed_passages(&passages).unwrap();
    let q = e.embed_query("ความจำของเอเจนต์").unwrap();
    assert_eq!(q.len(), 384);
    let scores: Vec<f32> = p.iter().map(|v| cosine(&q, v)).collect();
    assert!(scores[0] > scores[1] && scores[0] > scores[2], "{scores:?}");
}
```

Run: `nice -n 15 cargo build -j 6 -p alice-embed` (first build downloads the static ORT archive, ~2 min).
Expected: builds. Then once, on the owner's machine: `ALICE_MODELS_DIR=$HOME/.alice/models nice -n 15 cargo test -j 6 -p alice-embed --release -- --ignored` → PASS.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml crates/alice-core crates/alice-embed
git commit -m "feat(embed): embedder trait, deterministic fake, f16 vector storage and fastembed multilingual-e5-small provider

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 7: Vector index (usearch HNSW)

**Files:**
- Create: `crates/alice-core/src/vector.rs`
- Modify: `Cargo.toml` (workspace dep `usearch`), `crates/alice-core/Cargo.toml`, `crates/alice-core/src/lib.rs` (`pub mod vector;`)
- Test: inside `vector.rs`

**Interfaces:**
- Consumes: `store::embeddings::{VecKey, for_each, count}`
- Produces:
  - `vector::{encode_key(VecKey) -> u64, decode_key(u64) -> VecKey, KeyFilter { Memories, Chunks, Any }}`
  - `vector::VectorIndex::{new(dim: usize) -> Result<VectorIndex>, open(path: &Path, dim: usize) -> Result<VectorIndex>, upsert(&self, key: VecKey, v: &[f32]) -> Result<()>, remove(&self, key: VecKey) -> Result<()>, search(&self, query: &[f32], k: usize, filter: KeyFilter) -> Result<Vec<(VecKey, f32)>> /* similarity = 1 - cosine distance, best first */, len(&self) -> usize, save(&self, path: &Path) -> Result<()>, is_dirty(&self) -> bool, rebuild_from(conn: &Connection, model: &str, dim: usize) -> Result<VectorIndex>}`
  - `VectorIndex` is `Send + Sync` (shared via `Arc` between the worker and searches).

- [ ] **Step 1: Failing tests (`vector.rs`)**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{Embedder, FakeEmbedder};
    use std::sync::Arc;

    fn v(e: &FakeEmbedder, s: &str) -> Vec<f32> {
        e.embed_query(s).unwrap()
    }

    #[test]
    fn keys_round_trip() {
        for k in [VecKey::Memory(1), VecKey::Chunk(1), VecKey::Memory(9_000_000), VecKey::Chunk(0)] {
            assert_eq!(decode_key(encode_key(k)), k);
        }
    }

    #[test]
    fn upsert_search_filter_and_replace() {
        let e = FakeEmbedder::new(384);
        let idx = VectorIndex::new(384).unwrap();
        idx.upsert(VecKey::Memory(1), &v(&e, "sqlite writer lease")).unwrap();
        idx.upsert(VecKey::Chunk(1), &v(&e, "sqlite writer lease")).unwrap();
        idx.upsert(VecKey::Memory(2), &v(&e, "tom yum soup recipe")).unwrap();
        let hits = idx.search(&v(&e, "sqlite writer lease"), 2, KeyFilter::Memories).unwrap();
        assert_eq!(hits[0].0, VecKey::Memory(1));
        assert!(hits[0].1 > 0.99);
        assert!(hits.iter().all(|(k, _)| matches!(k, VecKey::Memory(_))));
        let chunks = idx.search(&v(&e, "sqlite"), 5, KeyFilter::Chunks).unwrap();
        assert_eq!(chunks.len(), 1);
        idx.upsert(VecKey::Memory(1), &v(&e, "completely different")).unwrap();
        assert_eq!(idx.len(), 3, "upsert replaces");
        idx.remove(VecKey::Memory(2)).unwrap();
        assert_eq!(idx.len(), 2);
        assert!(idx.upsert(VecKey::Memory(3), &[0.0; 10]).is_err(), "wrong dimension");
    }

    #[test]
    fn grows_past_initial_capacity_and_survives_save_open() {
        let e = FakeEmbedder::new(384);
        let idx = VectorIndex::new(384).unwrap();
        for i in 0..3_000 {
            idx.upsert(VecKey::Memory(i), &v(&e, &format!("note {i}"))).unwrap();
        }
        assert_eq!(idx.len(), 3_000);
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("vectors").join("m.usearch");
        idx.save(&path).unwrap();
        assert!(!idx.is_dirty());
        let back = VectorIndex::open(&path, 384).unwrap();
        assert_eq!(back.len(), 3_000);
        back.upsert(VecKey::Memory(5_000), &v(&e, "after load")).unwrap();
        assert_eq!(back.search(&v(&e, "after load"), 1, KeyFilter::Any).unwrap()[0].0, VecKey::Memory(5_000));
        assert!(VectorIndex::open(&d.path().join("missing.usearch"), 384).is_err());
    }

    #[test]
    fn many_threads_search_and_insert_without_errors() {
        let e = Arc::new(FakeEmbedder::new(384));
        let idx = Arc::new(VectorIndex::new(384).unwrap());
        let mut handles = Vec::new();
        for t in 0..200 {
            let (e, idx) = (e.clone(), idx.clone());
            handles.push(std::thread::spawn(move || {
                for i in 0..20 {
                    if t % 5 == 0 {
                        idx.upsert(VecKey::Chunk(t * 100 + i), &e.embed_query(&format!("t{t} i{i}")).unwrap()).unwrap();
                    } else {
                        idx.search(&e.embed_query("t1 i1").unwrap(), 5, KeyFilter::Any).unwrap();
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(idx.len(), 40 * 20);
    }

    #[test]
    fn rebuild_from_the_embeddings_table() {
        use crate::project::ProjectRef;
        use crate::store::{embeddings, memories, open_writer, projects};
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        let e = FakeEmbedder::new(384);
        for t in ["one", "two", "three"] {
            let m = memories::prepare(memories::NewMemoryInput { content: t.into(), ..Default::default() }, &p, "h".into()).unwrap();
            let memories::InsertOutcome::New { seq, .. } = memories::insert(&c, &m).unwrap() else { panic!() };
            embeddings::put(&c, VecKey::Memory(seq), "fake", &v(&e, t)).unwrap();
        }
        let idx = VectorIndex::rebuild_from(&c, "fake", 384).unwrap();
        assert_eq!(idx.len(), 3);
    }
}
```

- [ ] **Step 2: Run to confirm failure**

Run: `nice -n 15 cargo test -j 6 -p alice-core vector`
Expected: FAIL.

- [ ] **Step 3: Implement `vector.rs`**

Add `usearch = "2.26.4"` to the workspace and `usearch.workspace = true` to `alice-core`.

> On Linux with gcc 12 (Debian bookworm) usearch's default `numkong` SIMD feature fails to compile; CI uses ubuntu-latest (gcc 13+). If a build host has gcc 12, use `usearch = { version = "2.26.4", default-features = false }`.

```rust
//! HNSW vector index (usearch, cosine, int8). The `embeddings` table is the source of truth;
//! this index is a disposable accelerator that can always be rebuilt.
//!
//! usearch facts this wrapper handles (verified in the spike):
//! - capacity never grows by itself: `reserve` before adding, also after `load`;
//! - concurrent callers are limited by "thread slots": a gate keeps callers <= THREADS;
//! - updating a key means remove + add; duplicate keys are rejected.

use crate::error::{AliceError, Result};
use crate::store::embeddings::{self, VecKey};
use parking_lot::{Condvar, Mutex, RwLock};
use rusqlite::Connection;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

const THREADS: usize = 64;
const MIN_CAPACITY: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyFilter {
    Memories,
    Chunks,
    Any,
}

/// Memories use even keys, chunks odd keys.
pub fn encode_key(k: VecKey) -> u64 {
    match k {
        VecKey::Memory(s) => (s as u64) << 1,
        VecKey::Chunk(s) => ((s as u64) << 1) | 1,
    }
}

pub fn decode_key(k: u64) -> VecKey {
    let seq = (k >> 1) as i64;
    if k & 1 == 0 { VecKey::Memory(seq) } else { VecKey::Chunk(seq) }
}

fn options(dim: usize) -> IndexOptions {
    IndexOptions {
        dimensions: dim,
        metric: MetricKind::Cos,
        quantization: ScalarKind::I8,
        connectivity: 0,
        expansion_add: 0,
        expansion_search: 0,
        multi: false,
    }
}

fn err(e: impl std::fmt::Display) -> AliceError {
    AliceError::Other(format!("vector index: {e}"))
}

/// Limits concurrent index calls to the reserved thread slots.
struct Gate {
    busy: Mutex<usize>,
    cv: Condvar,
}

struct GateGuard<'a>(&'a Gate);

impl Gate {
    fn enter(&self) -> GateGuard<'_> {
        let mut busy = self.busy.lock();
        while *busy >= THREADS {
            self.cv.wait(&mut busy);
        }
        *busy += 1;
        GateGuard(self)
    }
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        *self.0.busy.lock() -= 1;
        self.0.cv.notify_one();
    }
}

pub struct VectorIndex {
    /// add/search/remove take the read lock; growing capacity takes the write lock.
    inner: RwLock<Index>,
    dim: usize,
    dirty: AtomicBool,
    gate: Gate,
}

impl VectorIndex {
    fn wrap(index: Index, dim: usize) -> Self {
        Self { inner: RwLock::new(index), dim, dirty: AtomicBool::new(false), gate: Gate { busy: Mutex::new(0), cv: Condvar::new() } }
    }

    pub fn new(dim: usize) -> Result<Self> {
        let index = Index::new(&options(dim)).map_err(err)?;
        index.reserve_capacity_and_threads(MIN_CAPACITY, THREADS).map_err(err)?;
        Ok(Self::wrap(index, dim))
    }

    /// Load a saved index. Fails if the file is missing, unreadable or has another dimension.
    pub fn open(path: &Path, dim: usize) -> Result<Self> {
        if !path.exists() {
            return Err(AliceError::NotFound(format!("vector index {}", path.display())));
        }
        let index = Index::new(&options(dim)).map_err(err)?;
        index.load(&path.to_string_lossy()).map_err(err)?;
        if index.dimensions() != dim {
            return Err(err(format!("dimension {} on disk, {dim} expected", index.dimensions())));
        }
        let cap = (index.size() * 2).max(MIN_CAPACITY);
        index.reserve_capacity_and_threads(cap, THREADS).map_err(err)?;
        Ok(Self::wrap(index, dim))
    }

    fn ensure_capacity(&self, extra: usize) -> Result<()> {
        {
            let i = self.inner.read();
            if i.size() + extra <= i.capacity() {
                return Ok(());
            }
        }
        let i = self.inner.write();
        let need = i.size() + extra;
        if need > i.capacity() {
            let cap = (i.capacity() * 2).max(need).max(MIN_CAPACITY);
            i.reserve_capacity_and_threads(cap, THREADS).map_err(err)?;
        }
        Ok(())
    }

    pub fn upsert(&self, key: VecKey, v: &[f32]) -> Result<()> {
        if v.len() != self.dim {
            return Err(AliceError::invalid(format!("vector has {} dimensions; the index uses {}", v.len(), self.dim)));
        }
        self.ensure_capacity(1)?;
        let _slot = self.gate.enter();
        let i = self.inner.read();
        let k = encode_key(key);
        if i.contains(k) {
            i.remove(k).map_err(err)?;
        }
        i.add(k, v).map_err(err)?;
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn remove(&self, key: VecKey) -> Result<()> {
        let _slot = self.gate.enter();
        self.inner.read().remove(encode_key(key)).map_err(err)?;
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Best first; similarity = 1 - cosine distance.
    pub fn search(&self, query: &[f32], k: usize, filter: KeyFilter) -> Result<Vec<(VecKey, f32)>> {
        if self.len() == 0 || k == 0 {
            return Ok(Vec::new());
        }
        let _slot = self.gate.enter();
        let i = self.inner.read();
        let m = match filter {
            KeyFilter::Any => i.search(query, k),
            KeyFilter::Memories => i.filtered_search(query, k, |key| key & 1 == 0),
            KeyFilter::Chunks => i.filtered_search(query, k, |key| key & 1 == 1),
        }
        .map_err(err)?;
        Ok(m.keys.iter().zip(m.distances.iter()).map(|(k, d)| (decode_key(*k), 1.0 - d)).collect())
    }

    pub fn len(&self) -> usize {
        self.inner.read().size()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    /// Atomic save: write a temp file, then rename over the old one.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("usearch.tmp");
        self.inner.read().save(&tmp.to_string_lossy()).map_err(err)?;
        std::fs::rename(&tmp, path)?;
        self.dirty.store(false, Ordering::Relaxed);
        Ok(())
    }

    pub fn rebuild_from(conn: &Connection, model: &str, dim: usize) -> Result<Self> {
        let idx = Self::new(dim)?;
        idx.ensure_capacity(embeddings::count(conn, model)?)?;
        let mut failed = 0usize;
        embeddings::for_each(conn, model, |key, v| {
            if idx.upsert(key, &v).is_err() {
                failed += 1;
            }
        })?;
        if failed > 0 {
            tracing::warn!(failed, "some vectors could not be added during rebuild");
        }
        Ok(idx)
    }
}
```

- [ ] **Step 4: Run tests**

Run: `nice -n 15 cargo test -j 6 -p alice-core vector`
Expected: PASS (the 200-thread test exercises the gate: without it usearch fails with "Reserve capacity ahead of searches!").

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml crates/alice-core
git commit -m "feat(core): usearch HNSW vector index with capacity growth, thread gate, atomic save and rebuild

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 8: Search pipeline (lexical, fusion, ranking, packing) and API types

**Files:**
- Create: `crates/alice-core/src/api.rs`, `crates/alice-core/src/search/{mod.rs,lexical.rs,fusion.rs,pack.rs}`
- Modify: `crates/alice-core/src/lib.rs` (`pub mod api; pub mod search;`)
- Test: inside each search module; `crates/alice-core/tests/search_quality.rs` with `crates/alice-core/tests/fixtures/search_gold.jsonl`

**Interfaces:**
- Consumes: `embed::Embedder`, `vector::{VectorIndex, KeyFilter}`, `store::{memories, sources, projects, kg}`, `model::*`, `normalize::*`, `codetokens::*`
- Produces (`api.rs`, all `Serialize + Deserialize + JsonSchema + Clone + Debug`):
  - `RecallMode { Fast, Precise }` (default Fast), `OutputFormat { Text, Json }` (default Text), `MatchVia { Vector, Words, Code }`, `Tier { Knowledge, Raw }`
  - `RecallRequest { query: String, project: Option<String>, topics: Vec<String>, kinds: Vec<Kind>, since: Option<String>, before: Option<String>, include_raw: bool, mode: RecallMode, max_tokens: Option<usize>, format: OutputFormat, limit: Option<usize>, #[schemars(skip)] cwd: Option<String> }`
  - `Hit { id: String, tier: Tier, kind: Option<Kind>, project: String, title: String, snippet: String, body: Option<String>, created_at: String, score: f64, matched: Vec<MatchVia>, status: Option<Status> }`
  - `RelatedRef { id: String, kind: Kind, title: String, via: String }`
  - `RecallResponse { project: Option<String>, results: Vec<Hit>, related: Vec<RelatedRef>, omitted: usize, raw_fallback: bool, text: Option<String> }`
  - `RememberRequest { content: String, kind: Option<Kind>, title: Option<String>, project: Option<String>, topics: Vec<String>, importance: Option<u8>, supersedes: Vec<String>, idempotency_key: Option<String>, #[schemars(skip)] cwd: Option<String>, #[schemars(skip)] author: Author }`
  - `RememberStatus { New, Duplicate, Spooled }`, `RememberResponse { id: Option<String>, status: RememberStatus, project: String }`
  - `ContextRequest { project: Option<String>, max_tokens: Option<usize>, #[schemars(skip)] cwd: Option<String> }`, `ContextResponse { project: String, text: String }`
  - `GetRequest { ids: Vec<String>, include_provenance: bool }`, `MemoryFull { memory: Memory, links: Vec<LinkRef>, provenance: Vec<String> }`, `LinkRef { id: String, link_type: String, direction: String }`, `GetResponse { memories: Vec<MemoryFull>, missing: Vec<String> }`
  - `UpdateRequest { id: String, #[serde(flatten)] update: MemoryUpdate }`
  - `IngestRequest { source: SourceInput, chunks: Vec<ChunkInput>, project: Option<String>, #[schemars(skip)] cwd: Option<String> }`
  - `Health { service: String /* "alice" */, instance: String, version: String, pid: u32, memories: u64, chunks: u64, projects: u64, embed_backlog: u64, read_only: Option<String>, index_ready: bool }`
- Produces (search):
  - `search::lexical::{word_terms(&str) -> Vec<String>, trigram_terms(&str) -> Vec<String>, code_terms(&str) -> Vec<String>, quote(&str) -> String, or_query(&[String]) -> Option<String>, should_use_trigram(&str) -> bool, memory_words(conn, q: &str, k: usize) -> Vec<i64>, memory_trigram(conn, q: &str, k: usize) -> Vec<i64>, memory_code(conn, q: &str, k: usize) -> Vec<i64>, chunk_words(conn, q, k) -> Vec<i64>, chunk_trigram(conn, q, k) -> Vec<i64>, chunk_code(conn, q, k) -> Vec<i64>, short_terms(&str) -> Vec<String>, memory_like(conn, q: &str, k: usize) -> Vec<i64>}` (all candidate functions return best-first sequence numbers and never fail: FTS errors are logged and yield an empty list)
  - `search::fusion::{ItemRef { Memory(i64), Chunk(i64) }, RRF_K: f64, rrf(lists: &[Vec<ItemRef>]) -> HashMap<ItemRef, f64>, Weights (fields: knowledge, raw, current_project, global_project, superseded, archived, usage_max; Default per spec), BoostInput { raw: bool, same_project: bool, is_global: bool, age_days: f64, half_life_days: Option<f64>, importance: u8, access_count: i64, status: Status }, boost(&Weights, &BoostInput) -> f64}`
  - `search::{SearchParams { query, project_filter_id: Option<String>, current_project_id: Option<String>, topics, kinds, since, before, include_raw, limit, relevance_floor: f32, raw_min_results: usize }, SearchOutput { hits: Vec<Hit>, related: Vec<RelatedRef>, raw_fallback: bool, memory_seqs: Vec<i64> }, run(conn, index: Option<&VectorIndex> /* None while the index is rebuilding: lexical only */, embedder: &dyn Embedder, weights: &Weights, p: &SearchParams, now: DateTime<Utc>) -> Result<SearchOutput>}`
  - `search::pack::{pack_text(out: &SearchOutput, project: Option<&str>, max_tokens: usize) -> (String, usize), pack_json(out: SearchOutput, project: Option<String>, max_tokens: usize) -> RecallResponse, make_snippet(text: &str, terms: &[String], max_chars: usize) -> String}`

- [ ] **Step 1: Write `api.rs`**

```rust
//! Request and response types shared by the HTTP API, MCP tools and the CLI.

use crate::kg_types::*;
use crate::model::{Author, Kind, Memory, Status};
use crate::store::memories::MemoryUpdate;
use crate::store::sources::{ChunkInput, SourceInput};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecallMode {
    /// Hybrid search without the judge (default).
    #[default]
    Fast,
    /// Re-rank the top results with the judge when one is configured.
    Precise,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatchVia {
    Vector,
    Words,
    Code,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Knowledge,
    Raw,
}

/// Search memories. Use it before answering questions about past work, decisions or bugs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RecallRequest {
    /// Keywords or a question.
    pub query: String,
    /// Project slug to restrict to. Defaults to boosting the current project without excluding others.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub topics: Vec<String>,
    #[serde(default)]
    pub kinds: Vec<Kind>,
    /// Only memories created on or after this date (YYYY-MM-DD or RFC 3339).
    #[serde(default)]
    pub since: Option<String>,
    /// Only memories created before this date.
    #[serde(default)]
    pub before: Option<String>,
    /// Also search raw session transcripts.
    #[serde(default)]
    pub include_raw: bool,
    #[serde(default)]
    pub mode: RecallMode,
    /// Response budget in tokens (default 1500).
    #[serde(default)]
    pub max_tokens: Option<usize>,
    #[serde(default)]
    pub format: OutputFormat,
    /// Number of results (default 10, max 50).
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Hit {
    pub id: String,
    pub tier: Tier,
    pub kind: Option<Kind>,
    pub project: String,
    pub title: String,
    pub snippet: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub created_at: String,
    pub score: f64,
    pub matched: Vec<MatchVia>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RelatedRef {
    pub id: String,
    pub kind: Kind,
    pub title: String,
    /// How it is related: a link type or `entity:<name>`.
    pub via: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RecallResponse {
    pub project: Option<String>,
    pub results: Vec<Hit>,
    pub related: Vec<RelatedRef>,
    pub omitted: usize,
    pub raw_fallback: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Save a memory: a decision, fix, fact, how-to, preference, warning or handoff worth keeping.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct RememberRequest {
    /// The memory text. The first line becomes the title unless `title` is given.
    pub content: String,
    #[serde(default)]
    pub kind: Option<Kind>,
    #[serde(default)]
    pub title: Option<String>,
    /// Project slug. Defaults to the current project.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub topics: Vec<String>,
    /// 1 (minor) to 5 (critical). Default 3.
    #[serde(default)]
    pub importance: Option<u8>,
    /// Ids of memories this one replaces.
    #[serde(default)]
    pub supersedes: Vec<String>,
    /// Repeat-safe key: a retry with the same key returns the first memory.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub cwd: Option<String>,
    #[serde(default)]
    #[schemars(skip)]
    pub author: Author,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RememberStatus {
    New,
    Duplicate,
    /// The daemon was unavailable; the request is queued on disk and will be stored shortly.
    Spooled,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RememberResponse {
    pub id: Option<String>,
    pub status: RememberStatus,
    pub project: String,
}

/// Briefing for the current project: key memories, warnings, recent decisions, last handoff, preferences.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ContextRequest {
    #[serde(default)]
    pub project: Option<String>,
    /// Budget in tokens (default 1200).
    #[serde(default)]
    pub max_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ContextResponse {
    pub project: String,
    pub text: String,
}

/// Full memories by id, with links and (optionally) the raw chunks they came from.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct GetRequest {
    pub ids: Vec<String>,
    #[serde(default)]
    pub include_provenance: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LinkRef {
    pub id: String,
    pub link_type: String,
    /// `out` when this memory points at `id`, `in` otherwise.
    pub direction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MemoryFull {
    pub memory: Memory,
    pub links: Vec<LinkRef>,
    pub provenance: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct GetResponse {
    pub memories: Vec<MemoryFull>,
    pub missing: Vec<String>,
}

/// Edit, supersede, archive, forget (soft delete) or restore a memory.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct UpdateRequest {
    pub id: String,
    #[serde(flatten)]
    pub update: MemoryUpdate,
}

/// Raw evidence ingest (used by capture hooks and importers).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct IngestRequest {
    pub source: SourceInput,
    pub chunks: Vec<ChunkInput>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Health {
    pub service: String,
    pub instance: String,
    pub version: String,
    pub pid: u32,
    pub memories: u64,
    pub chunks: u64,
    pub projects: u64,
    pub embed_backlog: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<String>,
    /// False while the vector index is being rebuilt (search is lexical-only meanwhile).
    pub index_ready: bool,
}

/// Facts about an entity: current, as of a date, or the full timeline.
pub type FactsRequest = FactsQuery;
/// Add, supersede or end a fact.
pub type AssertFactRequest = AssertFact;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FactsResponse {
    pub entity: String,
    pub facts: Vec<Fact>,
    pub text: String,
}
```

Add to `lib.rs` a re-export module so `api.rs` can name KG types without a long path:
```rust
pub mod kg_types {
    pub use crate::store::kg::{AssertFact, Fact, FactAction, FactOutcome, FactsQuery, ObjectKind};
}
```

- [ ] **Step 2: Failing tests for `lexical.rs` and `fusion.rs`**

`lexical.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectRef;
    use crate::store::{memories, open_writer, projects};

    #[test]
    fn terms_are_quoted_and_operators_are_inert() {
        assert_eq!(quote("a\"b"), "\"a\"\"b\"");
        let q = or_query(&word_terms("NEAR(foo AND -bar*) \"")).unwrap();
        assert_eq!(q, "\"near\" OR \"foo\" OR \"and\" OR \"bar\"");
        assert!(word_terms("😀 !!").is_empty());
        assert_eq!(or_query(&[]), None);
    }

    #[test]
    fn code_terms_come_from_identifiers() {
        assert!(word_terms("why HostLinkReconnect").contains(&"reconnect".to_string()));
        assert_eq!(code_terms("fix writer_lease now"), vec!["writerlease".to_string()]);
        assert!(code_terms("plain words").is_empty());
    }

    #[test]
    fn trigram_rules() {
        assert!(should_use_trigram("ความจำ"));
        assert!(should_use_trigram("lease timeout"));
        assert!(!should_use_trigram("why does the writer lease time out so often"));
        assert_eq!(trigram_terms("AI ความจำ go"), vec!["ความจำ".to_string()]);
    }

    fn db_with(texts: &[&str]) -> (tempfile::TempDir, rusqlite::Connection) {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        for t in texts {
            let m = memories::prepare(memories::NewMemoryInput { content: (*t).into(), ..Default::default() }, &p, "h".into()).unwrap();
            memories::insert(&c, &m).unwrap();
        }
        (d, c)
    }

    #[test]
    fn hostile_queries_never_error() {
        let (_d, c) = db_with(&["writer lease timeout", "ระบบความจำสำหรับเอเจนต์"]);
        for q in ["\"", "*", "a:b", "(", "NEAR", "AND OR NOT", "-", "AI", "ไทย", "😀", "''", "x\"y"] {
            let _ = memory_words(&c, q, 10);
            let _ = memory_trigram(&c, q, 10);
            let _ = memory_code(&c, q, 10);
        }
        assert_eq!(memory_trigram(&c, "ความจำ", 10).len(), 1);
        assert_eq!(memory_words(&c, "lease", 10).len(), 1);
        assert_eq!(memory_trigram(&c, "จำ", 10).len(), 0, "2 code points: below trigram minimum");
        assert_eq!(memory_like(&c, "จำ", 10).len(), 1, "LIKE fallback finds it");
        assert!(memory_like(&c, "%", 10).is_empty(), "ASCII and wildcards are not LIKE-scanned");
    }
}
```

`fusion.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;

    #[test]
    fn rrf_rewards_items_found_by_several_lists() {
        let a = ItemRef::Memory(1);
        let b = ItemRef::Memory(2);
        let s = rrf(&[vec![a, b], vec![a]]);
        assert!(s[&a] > s[&b]);
        assert!((s[&b] - 1.0 / 62.0).abs() < 1e-9);
    }

    fn base() -> BoostInput {
        BoostInput { raw: false, same_project: false, is_global: false, age_days: 0.0, half_life_days: None,
            importance: 3, access_count: 0, status: Status::Active }
    }

    #[test]
    fn boosts_follow_the_spec() {
        let w = Weights::default();
        let b0 = boost(&w, &base());
        assert!((b0 - 1.05).abs() < 1e-9, "importance 3 => 1.05, got {b0}");
        assert!((boost(&w, &BoostInput { raw: true, ..base() }) / b0 - 0.6).abs() < 1e-9);
        assert!((boost(&w, &BoostInput { same_project: true, ..base() }) / b0 - 1.3).abs() < 1e-9);
        assert!((boost(&w, &BoostInput { is_global: true, ..base() }) / b0 - 1.1).abs() < 1e-9);
        assert!((boost(&w, &BoostInput { status: Status::Superseded, ..base() }) / b0 - 0.3).abs() < 1e-9);
        let old = boost(&w, &BoostInput { age_days: 7.0, half_life_days: Some(7.0), ..base() }) / b0;
        assert!((old - (0.7 + 0.3 * (-1.0f64).exp())).abs() < 1e-9);
        let hot = boost(&w, &BoostInput { access_count: 1000, ..base() }) / b0;
        assert!((hot - 1.10).abs() < 1e-6);
    }
}
```

- [ ] **Step 3: Implement `lexical.rs`**

```rust
//! FTS5 query construction and candidate retrieval. Every user term is quoted, so FTS5
//! operators in the query are treated as literal words.

use crate::codetokens::{expand_code_tokens, is_codeish};
use rusqlite::Connection;

pub fn quote(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

pub fn or_query(terms: &[String]) -> Option<String> {
    (!terms.is_empty()).then(|| terms.iter().map(|t| quote(t)).collect::<Vec<_>>().join(" OR "))
}

fn push_unique(out: &mut Vec<String>, w: String) {
    if !w.is_empty() && w.chars().any(char::is_alphanumeric) && !out.contains(&w) {
        out.push(w);
    }
}

/// Lowercase words for the unicode61 index plus identifier parts. At most 32 terms.
pub fn word_terms(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    for w in query.split(|c: char| c.is_whitespace() || (c.is_ascii_punctuation() && c != '_')) {
        push_unique(&mut out, w.trim().to_lowercase());
    }
    for w in expand_code_tokens(query).split(' ') {
        push_unique(&mut out, w.to_string());
    }
    out.truncate(32);
    out
}

/// Joined identifiers (`HostLinkReconnect` -> `hostlinkreconnect`) for exact code matches.
pub fn code_terms(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    for tok in query.split_whitespace() {
        let tok = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '/' && c != '.');
        if tok.len() >= 3 && is_codeish(tok) {
            push_unique(&mut out, tok.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_lowercase());
        }
    }
    out
}

/// Substring terms for the trigram index: whitespace-separated pieces of at least 3 characters.
pub fn trigram_terms(query: &str) -> Vec<String> {
    let short_query = query.split_whitespace().count() <= 2;
    let mut out = Vec::new();
    for w in query.split_whitespace() {
        let w = w.trim_matches(|c: char| c.is_ascii_punctuation()).to_lowercase();
        let n = w.chars().count();
        if n >= 3 && (!w.is_ascii() || short_query) {
            push_unique(&mut out, w);
        }
    }
    out
}

/// Trigram search helps scripts without spaces and very short queries.
pub fn should_use_trigram(query: &str) -> bool {
    query.chars().any(|c| !c.is_ascii()) || query.split_whitespace().count() <= 2
}

fn run(conn: &Connection, sql: &str, fts_query: Option<String>, k: usize) -> Vec<i64> {
    let Some(q) = fts_query else { return Vec::new() };
    let result = (|| -> rusqlite::Result<Vec<i64>> {
        let mut stmt = conn.prepare_cached(sql)?;
        let rows = stmt.query_map(rusqlite::params![q, k as i64], |r| r.get(0))?;
        rows.collect()
    })();
    match result {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "fts query failed; treating as no lexical matches");
            Vec::new()
        }
    }
}

pub fn memory_words(conn: &Connection, q: &str, k: usize) -> Vec<i64> {
    run(conn, "SELECT rowid FROM memories_fts WHERE memories_fts MATCH ?1 ORDER BY bm25(memories_fts, 2.0, 1.0, 1.5, 1.5) LIMIT ?2", or_query(&word_terms(q)), k)
}

pub fn memory_trigram(conn: &Connection, q: &str, k: usize) -> Vec<i64> {
    run(conn, "SELECT rowid FROM memories_tri WHERE memories_tri MATCH ?1 ORDER BY bm25(memories_tri, 2.0, 1.0) LIMIT ?2", or_query(&trigram_terms(q)), k)
}

pub fn memory_code(conn: &Connection, q: &str, k: usize) -> Vec<i64> {
    let terms = code_terms(q);
    let fq = or_query(&terms).map(|o| format!("code_tokens : ({o})"));
    run(conn, "SELECT rowid FROM memories_fts WHERE memories_fts MATCH ?1 ORDER BY bm25(memories_fts) LIMIT ?2", fq, k)
}

pub fn chunk_words(conn: &Connection, q: &str, k: usize) -> Vec<i64> {
    run(conn, "SELECT rowid FROM chunks_fts WHERE chunks_fts MATCH ?1 ORDER BY bm25(chunks_fts, 1.0, 1.5) LIMIT ?2", or_query(&word_terms(q)), k)
}

pub fn chunk_trigram(conn: &Connection, q: &str, k: usize) -> Vec<i64> {
    run(conn, "SELECT rowid FROM chunks_tri WHERE chunks_tri MATCH ?1 ORDER BY bm25(chunks_tri) LIMIT ?2", or_query(&trigram_terms(q)), k)
}

pub fn chunk_code(conn: &Connection, q: &str, k: usize) -> Vec<i64> {
    let fq = or_query(&code_terms(q)).map(|o| format!("code_tokens : ({o})"));
    run(conn, "SELECT rowid FROM chunks_fts WHERE chunks_fts MATCH ?1 ORDER BY bm25(chunks_fts) LIMIT ?2", fq, k)
}

/// Non-ASCII terms of 1–2 code points (e.g. Thai "จำ" = จ + ำ) are below the trigram minimum.
pub fn short_terms(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    for w in query.split_whitespace() {
        let w = w.trim_matches(|c: char| c.is_ascii_punctuation()).to_lowercase();
        if !w.is_ascii() && (1..3).contains(&w.chars().count()) {
            push_unique(&mut out, w);
        }
    }
    out
}

fn like_pattern(term: &str) -> String {
    let escaped = term.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    format!("%{escaped}%")
}

/// LIKE scan of the knowledge tier for short non-ASCII terms (newest first). Never fails.
pub fn memory_like(conn: &Connection, q: &str, k: usize) -> Vec<i64> {
    let terms = short_terms(q);
    if terms.is_empty() {
        return Vec::new();
    }
    let clause = vec!["(title LIKE ? ESCAPE '\\' OR body LIKE ? ESCAPE '\\')"; terms.len()].join(" OR ");
    let sql = format!("SELECT seq FROM memories WHERE status <> 'trashed' AND ({clause}) ORDER BY seq DESC LIMIT {k}");
    let params: Vec<String> = terms.iter().flat_map(|t| {
        let p = like_pattern(t);
        [p.clone(), p]
    }).collect();
    let result = (|| -> rusqlite::Result<Vec<i64>> {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| r.get(0))?;
        rows.collect()
    })();
    result.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "like scan failed");
        Vec::new()
    })
}
```

> `trigram_terms` keeps non-ASCII pieces of ≥ 3 characters always, and ASCII pieces of ≥ 3 characters only for queries of one or two words (so long English questions rely on the word index). The test `trigram_terms("AI ความจำ go")` therefore returns only `ความจำ` (the query has three words).

- [ ] **Step 4: Implement `fusion.rs`**

```rust
//! Reciprocal rank fusion and ranking boosts. Pure functions.

use crate::model::Status;
use std::collections::HashMap;

pub const RRF_K: f64 = 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ItemRef {
    Memory(i64),
    Chunk(i64),
}

/// Sum of 1 / (k + rank) over every list an item appears in (rank starts at 1).
pub fn rrf(lists: &[Vec<ItemRef>]) -> HashMap<ItemRef, f64> {
    let mut scores = HashMap::new();
    for list in lists {
        for (i, item) in list.iter().enumerate() {
            *scores.entry(*item).or_insert(0.0) += 1.0 / (RRF_K + (i + 1) as f64);
        }
    }
    scores
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct Weights {
    pub knowledge: f64,
    pub raw: f64,
    pub current_project: f64,
    pub global_project: f64,
    pub superseded: f64,
    pub archived: f64,
    /// Maximum boost from frequent use (0.10 = +10 %).
    pub usage_max: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self { knowledge: 1.0, raw: 0.6, current_project: 1.3, global_project: 1.1, superseded: 0.3, archived: 0.5, usage_max: 0.10 }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BoostInput {
    pub raw: bool,
    pub same_project: bool,
    pub is_global: bool,
    pub age_days: f64,
    pub half_life_days: Option<f64>,
    pub importance: u8,
    pub access_count: i64,
    pub status: Status,
}

pub fn boost(w: &Weights, b: &BoostInput) -> f64 {
    let tier = if b.raw { w.raw } else { w.knowledge };
    let project = if b.same_project {
        w.current_project
    } else if b.is_global {
        w.global_project
    } else {
        1.0
    };
    let recency = match b.half_life_days {
        Some(h) if h > 0.0 => 0.7 + 0.3 * (-b.age_days.max(0.0) / h).exp(),
        _ => 1.0,
    };
    let importance = 0.9 + 0.05 * f64::from(b.importance);
    let usage_frac = ((1.0 + b.access_count.max(0) as f64).ln() / 1001f64.ln()).min(1.0);
    let usage = 1.0 + w.usage_max * usage_frac;
    let status = match b.status {
        Status::Superseded => w.superseded,
        Status::Archived => w.archived,
        _ => 1.0,
    };
    tier * project * recency * importance * usage * status
}
```

- [ ] **Step 5: Failing tests for the pipeline (`search/mod.rs`) and `pack.rs`**

`search/mod.rs` tests (use `FakeEmbedder` and an in-memory `VectorIndex`):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{Embedder, FakeEmbedder, memory_embed_text};
    use crate::model::{Kind, Status};
    use crate::project::ProjectRef;
    use crate::store::embeddings::VecKey;
    use crate::store::{memories, open_writer, projects, sources};
    use crate::vector::VectorIndex;

    struct Fx {
        _d: tempfile::TempDir,
        c: rusqlite::Connection,
        idx: VectorIndex,
        e: FakeEmbedder,
        app: projects::ResolvedProject,
        other: projects::ResolvedProject,
    }

    fn fx() -> Fx {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let mk = |k: &str, s: &str| ProjectRef { key: k.into(), slug: s.into(), name: s.into(), root: None, remote: None };
        let app = projects::upsert_project(&c, &mk("k/app", "app")).unwrap();
        let other = projects::upsert_project(&c, &mk("k/other", "other")).unwrap();
        Fx { _d: d, c, idx: VectorIndex::new(384).unwrap(), e: FakeEmbedder::new(384), app, other }
    }

    impl Fx {
        fn add(&self, p: &projects::ResolvedProject, kind: Kind, content: &str) -> (String, i64) {
            let m = memories::prepare(memories::NewMemoryInput { content: content.into(), kind: Some(kind), ..Default::default() }, p, "h".into()).unwrap();
            let memories::InsertOutcome::New { id, seq } = memories::insert(&self.c, &m).unwrap() else { panic!() };
            let v = self.e.embed_passages(&[memory_embed_text(&m.title, &m.body)]).unwrap().remove(0);
            self.idx.upsert(VecKey::Memory(seq), &v).unwrap();
            (id, seq)
        }

        fn params(&self, q: &str) -> SearchParams {
            SearchParams { query: q.into(), current_project_id: Some(self.app.id.clone()), limit: 10, relevance_floor: 0.82, raw_min_results: 3, ..Default::default() }
        }

        fn search(&self, p: &SearchParams) -> SearchOutput {
            run(&self.c, Some(&self.idx), &self.e, &Weights::default(), p, chrono::Utc::now()).unwrap()
        }
    }

    #[test]
    fn finds_thai_english_and_code_identifiers() {
        let f = fx();
        f.add(&f.app, Kind::Decision, "ใช้ SQLite แบบ WAL เพื่อให้ระบบความจำไม่ค้าง");
        f.add(&f.app, Kind::Bugfix, "HostLinkReconnect retried forever after sleep; capped at 5 attempts");
        f.add(&f.app, Kind::Note, "Lunch menu ideas");
        assert!(f.search(&f.params("ความจำ")).hits[0].title.contains("ความจำ"));
        let code = f.search(&f.params("reconnect"));
        assert!(code.hits[0].title.contains("HostLinkReconnect"));
        assert!(code.hits[0].matched.contains(&MatchVia::Code) || code.hits[0].matched.contains(&MatchVia::Words));
    }

    #[test]
    fn current_project_and_status_shape_the_ranking() {
        let f = fx();
        let (_o, _) = f.add(&f.other, Kind::Fact, "deploy target is easypanel");
        let (mine, _) = f.add(&f.app, Kind::Fact, "deploy target is easypanel server");
        let hits = f.search(&f.params("deploy target easypanel")).hits;
        assert_eq!(hits[0].id, mine, "current project first");
        memories::apply_update(&f.c, &mine, &memories::MemoryUpdate { action: memories::UpdateAction::Archive, ..Default::default() }, "h".into()).unwrap();
        let hits = f.search(&f.params("deploy target easypanel")).hits;
        assert_ne!(hits[0].id, mine, "archived drops below active");
        assert_eq!(hits.iter().find(|h| h.id == mine).unwrap().status, Some(Status::Archived));
    }

    #[test]
    fn trashed_memories_never_appear_and_filters_apply() {
        let f = fx();
        let (gone, _) = f.add(&f.app, Kind::Note, "secret plan alpha");
        f.add(&f.app, Kind::Decision, "alpha decision recorded");
        memories::apply_update(&f.c, &gone, &memories::MemoryUpdate { action: memories::UpdateAction::Forget, ..Default::default() }, "h".into()).unwrap();
        let out = f.search(&SearchParams { kinds: vec![Kind::Decision], ..f.params("alpha") });
        assert!(out.hits.iter().all(|h| h.id != gone));
        assert!(out.hits.iter().all(|h| h.kind == Some(Kind::Decision)));
        let restricted = f.search(&SearchParams { project_filter_id: Some(f.other.id.clone()), ..f.params("alpha") });
        assert!(restricted.hits.is_empty());
    }

    #[test]
    fn raw_fallback_when_knowledge_is_thin() {
        let f = fx();
        let p = &f.app;
        let out = sources::ingest(&f.c, p, &sources::SourceInput { uri: "s1".into(), ..Default::default() },
            &[sources::ChunkInput { ord: 0, text: "we discussed the frobnicator calibration".into(), ..Default::default() }]).unwrap();
        assert_eq!(out.inserted, 1);
        let res = f.search(&f.params("frobnicator"));
        assert!(res.raw_fallback);
        assert_eq!(res.hits[0].tier, Tier::Raw);
        let no_fallback = f.search(&SearchParams { raw_min_results: 0, ..f.params("frobnicator") });
        assert!(no_fallback.hits.is_empty());
    }

    #[test]
    fn hostile_and_empty_queries() {
        let f = fx();
        f.add(&f.app, Kind::Note, "anything");
        for q in ["\"", "NEAR(", "AI", "😀", "a:b*"] {
            assert!(run(&f.c, Some(&f.idx), &f.e, &Weights::default(), &f.params(q), chrono::Utc::now()).is_ok(), "{q}");
        }
        assert!(run(&f.c, Some(&f.idx), &f.e, &Weights::default(), &f.params("   "), chrono::Utc::now()).is_err());
    }

    #[test]
    fn related_follows_links() {
        let f = fx();
        let (old, _) = f.add(&f.app, Kind::Fact, "api port is 8080");
        let m = memories::prepare(memories::NewMemoryInput { content: "api port is 9090".into(), kind: Some(Kind::Fact), supersedes: vec![old.clone()], ..Default::default() }, &f.app, "h".into()).unwrap();
        memories::insert(&f.c, &m).unwrap();
        let out = f.search(&f.params("api port 9090"));
        assert!(out.related.iter().any(|r| r.id == old) || out.hits.iter().any(|h| h.id == old));
    }
}
```

`pack.rs` tests:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Hit, MatchVia, Tier};
    use crate::model::Kind;

    fn hit(i: usize, body: &str) -> Hit {
        Hit { id: format!("ID{i}"), tier: Tier::Knowledge, kind: Some(Kind::Decision), project: "app".into(),
            title: format!("title {i}"), snippet: body.chars().take(50).collect(), body: Some(body.into()),
            created_at: "2026-10-13T00:00:00.000Z".into(), score: 1.0, matched: vec![MatchVia::Words], status: None }
    }

    #[test]
    fn budget_limits_output_and_reports_omitted() {
        let out = SearchOutput { hits: (0..10).map(|i| hit(i, &"word ".repeat(200))).collect(), ..Default::default() };
        let (text, omitted) = pack_text(&out, Some("app"), 300);
        assert!(crate::normalize::estimate_tokens(&text) <= 330, "{}", text.len());
        assert!(omitted > 0);
        assert!(text.contains("[1] ID0"));
        assert!(text.contains("omitted"));
    }

    #[test]
    fn small_results_include_full_bodies() {
        let out = SearchOutput { hits: vec![hit(0, "short body")], ..Default::default() };
        let (text, omitted) = pack_text(&out, Some("app"), 1500);
        assert_eq!(omitted, 0);
        assert!(text.contains("short body"));
    }

    #[test]
    fn empty_results_say_so() {
        let (text, _) = pack_text(&SearchOutput::default(), Some("app"), 1500);
        assert!(text.contains("No memories matched"));
    }

    #[test]
    fn snippet_centres_on_the_first_match() {
        let text = format!("{} needle {}", "a".repeat(500), "b".repeat(500));
        let s = make_snippet(&text, &["needle".into()], 100);
        assert!(s.contains("needle"));
        assert!(s.starts_with('…'));
        assert!(s.chars().count() <= 102);
    }
}
```

- [ ] **Step 6: Implement `search/mod.rs`**

```rust
//! Recall pipeline: candidates -> fusion -> filters and boosts -> raw fallback -> related.

pub mod fusion;
pub mod lexical;
pub mod pack;

use crate::api::{Hit, MatchVia, RelatedRef, Tier};
use crate::embed::Embedder;
use crate::error::{AliceError, Result};
use crate::model::{Kind, RAW_HALF_LIFE_DAYS, Status};
use crate::store::projects::GLOBAL_ID;
use crate::store::{memories, sources};
use crate::vector::{KeyFilter, VectorIndex};
use crate::store::embeddings::VecKey;
use chrono::{DateTime, Utc};
use fusion::{BoostInput, ItemRef, Weights, boost, rrf};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};

const CANDIDATES: usize = 100;
const FILTERED_CANDIDATES: usize = 400;

#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub query: String,
    /// Restrict to this project id.
    pub project_filter_id: Option<String>,
    /// Boost this project id.
    pub current_project_id: Option<String>,
    pub topics: Vec<String>,
    pub kinds: Vec<Kind>,
    pub since: Option<String>,
    pub before: Option<String>,
    pub include_raw: bool,
    pub limit: usize,
    pub relevance_floor: f32,
    pub raw_min_results: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SearchOutput {
    pub hits: Vec<Hit>,
    pub related: Vec<RelatedRef>,
    pub raw_fallback: bool,
    /// Sequence numbers of returned memories (for access counting).
    pub memory_seqs: Vec<i64>,
}

#[derive(Default)]
struct Signals {
    via: HashSet<MatchVia>,
    lexical: bool,
    similarity: f32,
}

fn age_days(created_at: &str, now: DateTime<Utc>) -> f64 {
    DateTime::parse_from_rfc3339(created_at)
        .map(|t| (now - t.with_timezone(&Utc)).num_seconds() as f64 / 86_400.0)
        .unwrap_or(0.0)
}

fn in_window(created_at: &str, since: &Option<String>, before: &Option<String>) -> bool {
    since.as_deref().is_none_or(|s| created_at >= s) && before.as_deref().is_none_or(|b| created_at < b)
}

fn collect(
    lists: Vec<(MatchVia, Vec<ItemRef>)>,
    vector: Vec<(ItemRef, f32)>,
) -> (HashMap<ItemRef, f64>, HashMap<ItemRef, Signals>) {
    let mut signals: HashMap<ItemRef, Signals> = HashMap::new();
    let mut ranked: Vec<Vec<ItemRef>> = Vec::new();
    for (via, list) in &lists {
        for item in list {
            let s = signals.entry(*item).or_default();
            s.via.insert(*via);
            s.lexical = true;
        }
        ranked.push(list.clone());
    }
    for (item, sim) in &vector {
        let s = signals.entry(*item).or_default();
        s.via.insert(MatchVia::Vector);
        s.similarity = s.similarity.max(*sim);
    }
    ranked.push(vector.iter().map(|(i, _)| *i).collect());
    (rrf(&ranked), signals)
}

fn vector_hits(index: Option<&VectorIndex>, q: &[f32], k: usize, filter: KeyFilter) -> Result<Vec<(VecKey, f32)>> {
    match index {
        Some(i) => i.search(q, k, filter),
        None => Ok(Vec::new()),
    }
}

fn ordered_via(s: &Signals) -> Vec<MatchVia> {
    [MatchVia::Vector, MatchVia::Words, MatchVia::Code].into_iter().filter(|v| s.via.contains(v)).collect()
}

/// Run the recall pipeline. Fails only for an empty query or a database error.
pub fn run(
    conn: &Connection,
    index: Option<&VectorIndex>,
    embedder: &dyn Embedder,
    weights: &Weights,
    p: &SearchParams,
    now: DateTime<Utc>,
) -> Result<SearchOutput> {
    let normalized = crate::normalize::nfc(p.query.trim());
    let q = normalized.as_str();
    if q.is_empty() {
        return Err(AliceError::invalid("query is empty; pass keywords or a question"));
    }
    let limit = p.limit.clamp(1, crate::model::limits::MAX_RECALL_LIMIT);
    let filtered = p.project_filter_id.is_some() || !p.kinds.is_empty() || !p.topics.is_empty() || p.since.is_some() || p.before.is_some();
    let k = if filtered { FILTERED_CANDIDATES } else { CANDIDATES };
    let qvec = embedder.embed_query(q)?;
    let terms = lexical::word_terms(q);

    // Knowledge tier.
    let mut kn_lists = vec![
        (MatchVia::Words, lexical::memory_words(conn, q, k).into_iter().map(ItemRef::Memory).collect::<Vec<_>>()),
        (MatchVia::Code, lexical::memory_code(conn, q, k).into_iter().map(ItemRef::Memory).collect()),
    ];
    if lexical::should_use_trigram(q) || kn_lists[0].1.len() < 10 {
        kn_lists.push((MatchVia::Words, lexical::memory_trigram(conn, q, k).into_iter().map(ItemRef::Memory).collect()));
    }
    let like = lexical::memory_like(conn, q, k);
    if !like.is_empty() {
        kn_lists.push((MatchVia::Words, like.into_iter().map(ItemRef::Memory).collect()));
    }
    let kn_vec: Vec<(ItemRef, f32)> = vector_hits(index, &qvec, k, KeyFilter::Memories)?
        .into_iter()
        .filter_map(|(key, sim)| match key { VecKey::Memory(s) => Some((ItemRef::Memory(s), sim)), VecKey::Chunk(_) => None })
        .collect();
    let (mut scores, mut signals) = collect(kn_lists, kn_vec);

    let mut raw_fallback = false;
    let raw_candidates = |scores: &mut HashMap<ItemRef, f64>, signals: &mut HashMap<ItemRef, Signals>| -> Result<()> {
        let mut lists = vec![
            (MatchVia::Words, lexical::chunk_words(conn, q, k).into_iter().map(ItemRef::Chunk).collect::<Vec<_>>()),
            (MatchVia::Code, lexical::chunk_code(conn, q, k).into_iter().map(ItemRef::Chunk).collect()),
        ];
        if lexical::should_use_trigram(q) || lists[0].1.len() < 10 {
            lists.push((MatchVia::Words, lexical::chunk_trigram(conn, q, k).into_iter().map(ItemRef::Chunk).collect()));
        }
        let vec: Vec<(ItemRef, f32)> = vector_hits(index, &qvec, k, KeyFilter::Chunks)?
            .into_iter()
            .filter_map(|(key, sim)| match key { VecKey::Chunk(s) => Some((ItemRef::Chunk(s), sim)), VecKey::Memory(_) => None })
            .collect();
        let (s2, sig2) = collect(lists, vec);
        scores.extend(s2);
        signals.extend(sig2);
        Ok(())
    };
    if p.include_raw {
        raw_candidates(&mut scores, &mut signals)?;
    }

    // Only relevant results are returned: a lexical match or vector similarity >= relevance_floor.
    let mut hits = score_items(conn, &scores, &signals, weights, p, now, &terms)?;
    hits.retain(|(_, relevant)| *relevant);
    let knowledge_relevant = hits.iter().filter(|(h, _)| h.tier == Tier::Knowledge).count();
    if !p.include_raw && knowledge_relevant < p.raw_min_results {
        let mut raw_scores = HashMap::new();
        let mut raw_signals = HashMap::new();
        raw_candidates(&mut raw_scores, &mut raw_signals)?;
        let extra: Vec<_> = score_items(conn, &raw_scores, &raw_signals, weights, p, now, &terms)?
            .into_iter()
            .filter(|(_, relevant)| *relevant)
            .collect();
        raw_fallback = !extra.is_empty();
        hits.extend(extra);
    }
    hits.truncate(limit);
    let hits: Vec<Hit> = hits.into_iter().map(|(h, _)| h).collect();
    let memory_ids: Vec<String> = hits.iter().filter(|h| h.tier == Tier::Knowledge).map(|h| h.id.clone()).collect();
    let memory_seqs = memory_ids.iter().filter_map(|id| memories::seq_of(conn, id).ok().flatten()).collect();
    let related = related_for(conn, &memory_ids)?;
    Ok(SearchOutput { hits, related, raw_fallback, memory_seqs })
}

/// Fetch rows, apply filters and boosts, return (hit, is_relevant) sorted best first.
fn score_items(
    conn: &Connection,
    scores: &HashMap<ItemRef, f64>,
    signals: &HashMap<ItemRef, Signals>,
    w: &Weights,
    p: &SearchParams,
    now: DateTime<Utc>,
    terms: &[String],
) -> Result<Vec<(Hit, bool)>> {
    let mut mem_seqs: Vec<i64> = Vec::new();
    let mut chunk_seqs: Vec<i64> = Vec::new();
    for item in scores.keys() {
        match item {
            ItemRef::Memory(s) => mem_seqs.push(*s),
            ItemRef::Chunk(s) => chunk_seqs.push(*s),
        }
    }
    let project_slug_by_id = project_slugs(conn)?;
    let project_id_by_slug: HashMap<&str, &str> =
        project_slug_by_id.iter().map(|(id, slug)| (slug.as_str(), id.as_str())).collect();
    let mut out: Vec<(Hit, bool, f64)> = Vec::new();
    let empty = Signals::default();
    for (seq, m) in memories::get_by_seqs(conn, &mem_seqs)? {
        if m.status == Status::Trashed {
            continue;
        }
        let project_id = project_id_by_slug.get(m.project.as_str()).copied().unwrap_or("");
        if p.project_filter_id.as_deref().is_some_and(|f| f != project_id) {
            continue;
        }
        if !p.kinds.is_empty() && !p.kinds.contains(&m.kind) {
            continue;
        }
        if !p.topics.is_empty() && !p.topics.iter().any(|t| m.topics.contains(t)) {
            continue;
        }
        if !in_window(&m.created_at, &p.since, &p.before) {
            continue;
        }
        let item = ItemRef::Memory(seq);
        let s = signals.get(&item).unwrap_or(&empty);
        let b = boost(w, &BoostInput {
            raw: false,
            same_project: p.current_project_id.as_deref() == Some(project_id),
            is_global: project_id == GLOBAL_ID,
            age_days: age_days(&m.created_at, now),
            half_life_days: m.kind.half_life_days(),
            importance: m.importance,
            access_count: m.access_count,
            status: m.status,
        });
        let score = scores[&item] * b;
        let relevant = s.lexical || s.similarity >= p.relevance_floor;
        let status = (m.status != Status::Active && m.status != Status::Pending).then_some(m.status);
        out.push((
            Hit {
                snippet: pack::make_snippet(&m.body, terms, 240),
                id: m.id,
                tier: Tier::Knowledge,
                kind: Some(m.kind),
                project: m.project,
                title: m.title,
                body: Some(m.body),
                created_at: m.created_at,
                score,
                matched: ordered_via(s),
                status,
            },
            relevant,
            score,
        ));
    }
    for c in sources::get_chunks_by_seqs(conn, &chunk_seqs)? {
        if p.project_filter_id.as_deref().is_some_and(|f| f != c.project_id) {
            continue;
        }
        let when = c.authored_at.clone().unwrap_or(c.created_at.clone());
        if !in_window(&when, &p.since, &p.before) {
            continue;
        }
        let item = ItemRef::Chunk(c.seq);
        let s = signals.get(&item).unwrap_or(&empty);
        let b = boost(w, &BoostInput {
            raw: true,
            same_project: p.current_project_id.as_deref() == Some(c.project_id.as_str()),
            is_global: c.project_id == GLOBAL_ID,
            age_days: age_days(&when, now),
            half_life_days: Some(RAW_HALF_LIFE_DAYS),
            importance: 3,
            access_count: 0,
            status: Status::Active,
        });
        let score = scores[&item] * b;
        let relevant = s.lexical || s.similarity >= p.relevance_floor;
        out.push((
            Hit {
                id: c.id,
                tier: Tier::Raw,
                kind: None,
                project: project_slug_by_id.get(&c.project_id).cloned().unwrap_or_default(),
                title: format!("raw · {}", c.source_id),
                snippet: pack::make_snippet(&c.text, terms, 240),
                body: Some(c.text),
                created_at: when,
                score,
                matched: ordered_via(s),
                status: None,
            },
            relevant,
            score,
        ));
    }
    out.sort_by(|a, b| b.2.total_cmp(&a.2));
    Ok(out.into_iter().map(|(h, r, _)| (h, r)).collect())
}

fn project_slugs(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt = conn.prepare_cached("SELECT id, slug FROM projects")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Up to five memories one hop from the given ones, through links or shared entities.
fn related_for(conn: &Connection, ids: &[String]) -> Result<Vec<RelatedRef>> {
    let top: Vec<&String> = ids.iter().take(3).collect();
    let exclude: HashSet<&String> = ids.iter().collect();
    let mut out: Vec<RelatedRef> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for id in top {
        let mut stmt = conn.prepare_cached(
            "SELECT m.id, m.kind, m.title, l.type FROM links l JOIN memories m ON m.id = CASE WHEN l.from_id = ?1 THEN l.to_id ELSE l.from_id END
             WHERE (l.from_id = ?1 OR l.to_id = ?1) AND m.status <> 'trashed' LIMIT 5",
        )?;
        let rows = stmt.query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?;
        for r in rows {
            let (rid, kind, title, via) = r?;
            if !exclude.contains(&rid) && seen.insert(rid.clone()) {
                out.push(RelatedRef { id: rid, kind: Kind::parse(&kind).unwrap_or(Kind::Note), title, via });
            }
        }
        let mut stmt = conn.prepare_cached(
            "SELECT m.id, m.kind, m.title, e.name FROM mentions a JOIN mentions b ON a.entity_id = b.entity_id AND b.memory_id <> a.memory_id
             JOIN memories m ON m.id = b.memory_id JOIN entities e ON e.id = a.entity_id
             WHERE a.memory_id = ?1 AND m.status <> 'trashed' ORDER BY m.importance DESC, m.updated_at DESC LIMIT 5",
        )?;
        let rows = stmt.query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?;
        for r in rows {
            let (rid, kind, title, entity) = r?;
            if !exclude.contains(&rid) && seen.insert(rid.clone()) {
                out.push(RelatedRef { id: rid, kind: Kind::parse(&kind).unwrap_or(Kind::Note), title, via: format!("entity:{entity}") });
            }
        }
    }
    out.truncate(5);
    Ok(out)
}
```

> Raw results found by the fallback are appended after the knowledge results (spec §4.4 step 5); with `include_raw = true` both tiers are fused and sorted together, raw weighted 0.6.

- [ ] **Step 7: Implement `search/pack.rs`**

```rust
//! Fit search results into a token budget, as compact text (default) or JSON.

use super::SearchOutput;
use crate::api::{Hit, MatchVia, RecallResponse, Tier};
use crate::normalize::{estimate_tokens, truncate_chars};

const FULL_BODY_CHARS: usize = 2_000;

/// A window of `max_chars` around the first occurrence of any term, on one line.
pub fn make_snippet(text: &str, terms: &[String], max_chars: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = flat.to_lowercase();
    let pos_byte = terms.iter().filter_map(|t| lower.find(t.as_str())).min();
    let start_char = match pos_byte {
        Some(b) => lower[..b].chars().count().saturating_sub(60),
        None => 0,
    };
    let body: String = flat.chars().skip(start_char).collect();
    let mut s = truncate_chars(&body, max_chars);
    if start_char > 0 {
        s.insert(0, '…');
    }
    s
}

fn via_label(v: &[MatchVia]) -> String {
    v.iter().map(|m| match m { MatchVia::Vector => "vector", MatchVia::Words => "words", MatchVia::Code => "code" }).collect::<Vec<_>>().join("+")
}

fn header(i: usize, h: &Hit) -> String {
    let kind = match (h.tier, h.kind) {
        (Tier::Raw, _) => "raw".to_string(),
        (_, Some(k)) => k.as_str().to_string(),
        _ => "note".to_string(),
    };
    let status = h.status.map(|s| format!(" · {}", s.as_str())).unwrap_or_default();
    format!("[{}] {} · {} · {} · {}{} · matched: {}\n", i + 1, h.id, kind, h.project, &h.created_at[..h.created_at.len().min(10)], status, via_label(&h.matched))
}

/// Text for agents. Returns the text and how many results did not fit.
pub fn pack_text(out: &SearchOutput, project: Option<&str>, max_tokens: usize) -> (String, usize) {
    if out.hits.is_empty() {
        let p = project.map(|p| format!(" in {p} or elsewhere")).unwrap_or_default();
        return (format!("No memories matched{p}. Try other words, include_raw=true, or remember this once you learn it.\n"), 0);
    }
    let mut blocks: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut omitted = 0usize;
    for (i, h) in out.hits.iter().enumerate() {
        let block = format!("{}{}\n{}\n\n", header(i, h), h.title, h.snippet);
        let cost = estimate_tokens(&block);
        if used + cost > max_tokens {
            omitted = out.hits.len() - i;
            break;
        }
        used += cost;
        blocks.push(block);
    }
    // Upgrade the best results to full bodies while the budget allows.
    for (i, h) in out.hits.iter().enumerate().take(blocks.len()) {
        let Some(body) = &h.body else { continue };
        let full = format!("{}{}\n{}\n\n", header(i, h), h.title, truncate_chars(body, FULL_BODY_CHARS));
        let delta = estimate_tokens(&full).saturating_sub(estimate_tokens(&blocks[i]));
        if used + delta <= max_tokens {
            used += delta;
            blocks[i] = full;
        }
    }
    let mut text: String = blocks.concat();
    if out.raw_fallback {
        text.push_str("(Few saved memories matched, so raw session excerpts are included.)\n");
    }
    if !out.related.is_empty() {
        let mut rel = String::from("related:\n");
        for r in &out.related {
            rel.push_str(&format!("- {} ({}) {} [{}]\n", r.id, r.kind.as_str(), r.title, r.via));
        }
        if used + estimate_tokens(&rel) <= max_tokens {
            text.push_str(&rel);
        }
    }
    if omitted > 0 {
        text.push_str(&format!("({omitted} more results omitted; raise max_tokens or narrow the query.)\n"));
    }
    (text, omitted)
}

/// Structured output with the same budget rule: bodies are dropped from results that do not fit.
pub fn pack_json(out: SearchOutput, project: Option<String>, max_tokens: usize) -> RecallResponse {
    let mut used = 0usize;
    let mut results = Vec::new();
    let mut omitted = 0usize;
    let total = out.hits.len();
    for (i, mut h) in out.hits.into_iter().enumerate() {
        let base = estimate_tokens(&h.title) + estimate_tokens(&h.snippet) + 30;
        if used + base > max_tokens {
            omitted = total - i;
            break;
        }
        used += base;
        if let Some(b) = h.body.take() {
            let b = truncate_chars(&b, FULL_BODY_CHARS);
            let cost = estimate_tokens(&b);
            if used + cost <= max_tokens {
                used += cost;
                h.body = Some(b);
            }
        }
        results.push(h);
    }
    RecallResponse { project, results, related: out.related, omitted, raw_fallback: out.raw_fallback, text: None }
}
```

- [ ] **Step 8: Search-quality fixture test**

Create `crates/alice-core/tests/fixtures/search_gold.jsonl` — each line is either a memory to load (`{"m": ..., "kind": ..., "id": ...}`) or a query with the expected memory id (`{"q": ..., "want": ...}`):
```jsonl
{"id":"wal","kind":"decision","m":"Use SQLite in WAL mode with a single writer thread so readers never block"}
{"id":"lease","kind":"bugfix","m":"Hub writer lease held during transcript mining caused 120 s diary timeouts; mining now runs off the writer"}
{"id":"thai-mem","kind":"fact","m":"ระบบความจำของเอเจนต์ต้องค้นหาภาษาไทยได้โดยไม่ต้องเว้นวรรค"}
{"id":"reconnect","kind":"bugfix","m":"HostLinkReconnect looped forever after sleep; capped at five attempts with backoff"}
{"id":"deploy","kind":"fact","m":"Production deploys go to Easypanel at 43.229.151.35 via the Coolify-compatible API"}
{"id":"pref-thai","kind":"preference","m":"Always reply to the owner in Thai; code and commit messages stay in English"}
{"id":"proto","kind":"decision","m":"v4 game frames must be framed protobuf; plain JSON fails with data read failed"}
{"id":"socks","kind":"warning","m":"LINE proxy must be socks5; http proxies break HTTP/2 push"}
{"id":"money","kind":"decision","m":"Money values are strings in the API to avoid float rounding"}
{"id":"rtk","kind":"howto","m":"Use rtk proxy git -C when working in another worktree because rtk drops the -C flag"}
{"q":"why do diary writes time out","want":"lease"}
{"q":"writer_lease","want":"lease"}
{"q":"ค้นหาภาษาไทย","want":"thai-mem"}
{"q":"ความจำ เอเจนต์","want":"thai-mem"}
{"q":"reconnect loop after sleep","want":"reconnect"}
{"q":"HostLinkReconnect","want":"reconnect"}
{"q":"where do we deploy production","want":"deploy"}
{"q":"which language should replies use","want":"pref-thai"}
{"q":"protobuf frames v4","want":"proto"}
{"q":"proxy type for LINE","want":"socks"}
{"q":"float rounding money","want":"money"}
{"q":"git -C flag lost","want":"rtk"}
{"q":"readers block writer sqlite","want":"wal"}
```

`crates/alice-core/tests/search_quality.rs`:
```rust
//! Synthetic multilingual gold set. Uses the fake embedder, so it measures the lexical
//! side and the fusion logic; the real-model run lives in `alice bench --quality`.

use alice_core::embed::{Embedder, FakeEmbedder, memory_embed_text};
use alice_core::model::Kind;
use alice_core::project::ProjectRef;
use alice_core::search::{SearchParams, fusion::Weights, run};
use alice_core::store::embeddings::VecKey;
use alice_core::store::{memories, open_writer, projects};
use alice_core::vector::VectorIndex;
use std::collections::HashMap;

#[test]
fn gold_queries_rank_the_expected_memory_high() {
    let d = tempfile::tempdir().unwrap();
    let c = open_writer(&d.path().join("a.db")).unwrap();
    let p = projects::upsert_project(&c, &ProjectRef { key: "k".into(), slug: "app".into(), name: "app".into(), root: None, remote: None }).unwrap();
    let e = FakeEmbedder::new(384);
    let idx = VectorIndex::new(384).unwrap();
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut queries = Vec::new();
    for line in include_str!("fixtures/search_gold.jsonl").lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        if let Some(text) = v["m"].as_str() {
            let kind = Kind::parse(v["kind"].as_str().unwrap()).unwrap();
            let m = memories::prepare(memories::NewMemoryInput { content: text.into(), kind: Some(kind), ..Default::default() }, &p, "h".into()).unwrap();
            let memories::InsertOutcome::New { id, seq } = memories::insert(&c, &m).unwrap() else { panic!() };
            idx.upsert(VecKey::Memory(seq), &e.embed_passages(&[memory_embed_text(&m.title, &m.body)]).unwrap()[0]).unwrap();
            ids.insert(v["id"].as_str().unwrap().into(), id);
        } else {
            queries.push((v["q"].as_str().unwrap().to_string(), v["want"].as_str().unwrap().to_string()));
        }
    }
    let mut reciprocal_ranks = Vec::new();
    for (q, want) in &queries {
        let params = SearchParams { query: q.clone(), current_project_id: Some(p.id.clone()), limit: 10, relevance_floor: 0.82, raw_min_results: 0, ..Default::default() };
        let out = run(&c, Some(&idx), &e, &Weights::default(), &params, chrono::Utc::now()).unwrap();
        let rank = out.hits.iter().position(|h| h.id == ids[want]).map(|i| i + 1);
        reciprocal_ranks.push(rank.map(|r| 1.0 / r as f64).unwrap_or(0.0));
        assert!(rank.is_some_and(|r| r <= 3), "query {q:?}: expected {want} in top 3, got rank {rank:?}");
    }
    let mrr = reciprocal_ranks.iter().sum::<f64>() / reciprocal_ranks.len() as f64;
    assert!(mrr >= 0.8, "MRR {mrr:.2} below 0.8");
}
```

- [ ] **Step 9: Run all core tests**

Run: `nice -n 15 cargo test -j 6 -p alice-core`
Expected: PASS. If a gold query misses, fix the pipeline (term handling, trigram rule, weights), not the fixture; record any intentional fixture change in the commit message.

- [ ] **Step 10: Commit**

```bash
git add crates/alice-core
git commit -m "feat(core): hybrid recall pipeline with Thai trigram, code identifiers, RRF, boosts, raw fallback, related and token-budget packing

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 9: Context briefing

**Files:**
- Create: `crates/alice-core/src/context.rs`
- Modify: `crates/alice-core/src/lib.rs` (`pub mod context;`)
- Test: inside `context.rs`

**Interfaces:**
- Consumes: `store::{memories, projects}`, `normalize::{estimate_tokens, truncate_chars}`, `model::*`
- Produces: `context::build(conn: &Connection, project: &ResolvedProject, max_tokens: usize, now: DateTime<Utc>) -> Result<String>`

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;
    use crate::project::ProjectRef;
    use crate::store::{memories, open_writer, projects};

    fn add(c: &rusqlite::Connection, p: &projects::ResolvedProject, kind: Kind, imp: u8, text: &str) {
        let m = memories::prepare(memories::NewMemoryInput { content: text.into(), kind: Some(kind), importance: Some(imp), ..Default::default() }, p, "h".into()).unwrap();
        memories::insert(c, &m).unwrap();
    }

    #[test]
    fn briefing_has_sections_in_order_and_respects_budget() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let app = projects::upsert_project(&c, &ProjectRef { key: "k".into(), slug: "app".into(), name: "app".into(), root: None, remote: None }).unwrap();
        let global = projects::upsert_project(&c, &ProjectRef::global()).unwrap();
        add(&c, &app, Kind::Handoff, 3, "Handoff: M1 tasks 1-5 done\nnext: vector index");
        add(&c, &app, Kind::Warning, 4, "Never commit alice.db to git");
        add(&c, &app, Kind::Decision, 5, "Rust single binary");
        add(&c, &app, Kind::Decision, 3, "FTS trigram for Thai");
        add(&c, &global, Kind::Preference, 4, "Reply in Thai");
        add(&c, &app, Kind::Note, 1, "trivial note");
        let text = build(&c, &app, 1_200, chrono::Utc::now()).unwrap();
        let pos = |s: &str| text.find(s).unwrap_or_else(|| panic!("missing {s}:\n{text}"));
        assert!(pos("Last handoff") < pos("Warnings"));
        assert!(pos("Warnings") < pos("Key memories"));
        assert!(pos("Key memories") < pos("Recent decisions"));
        assert!(pos("Recent decisions") < pos("Your preferences"));
        assert!(text.contains("Reply in Thai"));
        assert!(!text.contains("trivial note"));
        let small = build(&c, &app, 60, chrono::Utc::now()).unwrap();
        assert!(estimate_tokens(&small) <= 75, "{small}");
    }

    #[test]
    fn empty_project_gets_a_hint() {
        let d = tempfile::tempdir().unwrap();
        let c = open_writer(&d.path().join("a.db")).unwrap();
        let p = projects::upsert_project(&c, &ProjectRef { key: "k".into(), slug: "new".into(), name: "new".into(), root: None, remote: None }).unwrap();
        assert!(build(&c, &p, 1_200, chrono::Utc::now()).unwrap().contains("No memories yet"));
    }
}
```

- [ ] **Step 2: Implement `context.rs`**

```rust
//! Token-budgeted briefing for a project.

use crate::error::Result;
use crate::model::Memory;
use crate::normalize::{estimate_tokens, truncate_chars};
use crate::store::memories::get_by_seqs;
use crate::store::projects::{GLOBAL_ID, ResolvedProject};
use chrono::{DateTime, Duration, Utc};
use rusqlite::Connection;

fn seqs(conn: &Connection, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(args, |r| r.get(0))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn line(m: &Memory) -> String {
    let first_body_line = m.body.lines().skip(1).find(|l| !l.trim().is_empty()).unwrap_or("");
    let detail = if first_body_line.is_empty() { String::new() } else { format!(" — {}", truncate_chars(first_body_line.trim(), 120)) };
    format!("- [{}] {}{} ({})\n", m.kind.as_str(), m.title, detail, m.id)
}

const LIVE: &str = "status IN ('active','pending')";

pub fn build(conn: &Connection, project: &ResolvedProject, max_tokens: usize, now: DateTime<Utc>) -> Result<String> {
    let total: i64 = conn.query_row(
        &format!("SELECT count(*) FROM memories WHERE project_id = ?1 AND {LIVE}"),
        [&project.id],
        |r| r.get(0),
    )?;
    let mut out = format!("# Context · {}\n", project.slug);
    if total == 0 && project.id != GLOBAL_ID {
        out.push_str(&format!(
            "No memories yet for {}. Save decisions, fixes and lessons with remember; use recall to search everything.\n",
            project.slug
        ));
    }
    let mut used = estimate_tokens(&out);
    let mut shown: Vec<i64> = Vec::new();
    let section = |title: &str, seq_list: Vec<i64>, full_first: bool, out: &mut String, used: &mut usize, shown: &mut Vec<i64>| -> Result<()> {
        let fresh: Vec<i64> = seq_list.into_iter().filter(|s| !shown.contains(s)).collect();
        if fresh.is_empty() {
            return Ok(());
        }
        let heading = format!("\n## {title}\n");
        if *used + estimate_tokens(&heading) > max_tokens {
            return Ok(());
        }
        let mut body = String::new();
        let mut body_cost = 0;
        for (i, (seq, m)) in get_by_seqs(conn, &fresh)?.into_iter().enumerate() {
            let l = if full_first && i == 0 {
                format!("{} · {} · {}\n{}\n", m.title, &m.created_at[..10], m.author.agent.clone().unwrap_or_default(), truncate_chars(&m.body, 600))
            } else {
                line(&m)
            };
            let cost = estimate_tokens(&l);
            if *used + estimate_tokens(&heading) + body_cost + cost > max_tokens {
                break;
            }
            body_cost += cost;
            body.push_str(&l);
            shown.push(seq);
        }
        if !body.is_empty() {
            out.push_str(&heading);
            out.push_str(&body);
            *used += estimate_tokens(&heading) + body_cost;
        }
        Ok(())
    };
    let pid = project.id.as_str();
    let since = (now - Duration::days(14)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    section("Last handoff", seqs(conn, &format!("SELECT seq FROM memories WHERE project_id=?1 AND kind='handoff' AND {LIVE} ORDER BY created_at DESC LIMIT 1"), &[&pid])?, true, &mut out, &mut used, &mut shown)?;
    section("Warnings", seqs(conn, &format!("SELECT seq FROM memories WHERE project_id=?1 AND kind='warning' AND {LIVE} ORDER BY importance DESC, updated_at DESC LIMIT 5"), &[&pid])?, false, &mut out, &mut used, &mut shown)?;
    section("Key memories", seqs(conn, &format!("SELECT seq FROM memories WHERE project_id=?1 AND importance >= 4 AND kind NOT IN ('handoff','diary') AND {LIVE} ORDER BY importance DESC, updated_at DESC LIMIT 8"), &[&pid])?, false, &mut out, &mut used, &mut shown)?;
    section("Recent decisions", seqs(conn, &format!("SELECT seq FROM memories WHERE project_id=?1 AND kind='decision' AND created_at >= ?2 AND {LIVE} ORDER BY created_at DESC LIMIT 5"), &[&pid, &since])?, false, &mut out, &mut used, &mut shown)?;
    if project.id != GLOBAL_ID {
        section("Your preferences", seqs(conn, &format!("SELECT seq FROM memories WHERE project_id=?1 AND kind='preference' AND {LIVE} ORDER BY importance DESC, updated_at DESC LIMIT 8"), &[&GLOBAL_ID])?, false, &mut out, &mut used, &mut shown)?;
    }
    Ok(out)
}
```

> The "Recent decisions" test inserts decisions created now, so the 14-day window includes them; "Rust single binary" (importance 5) appears under Key memories and is not repeated under Recent decisions.

- [ ] **Step 3: Run tests**

Run: `nice -n 15 cargo test -j 6 -p alice-core context`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/alice-core
git commit -m "feat(core): token-budgeted project context briefing

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 10: Daemon core — paths, writer thread, embedding worker, Service

**Files:**
- Create: `crates/alice-daemon/Cargo.toml`, `crates/alice-daemon/src/{lib.rs,paths.rs,writer.rs,worker.rs,service.rs}`
- Modify: `Cargo.toml` (members += `crates/alice-daemon`; workspace deps `tokio`, `crossbeam-channel`)
- Test: inside `writer.rs`, `service.rs`; `crates/alice-daemon/tests/service_concurrency.rs`

**Interfaces:**
- Consumes: everything in `alice-core`; `alice_core::embed::Embedder`; `alice_core::vector::VectorIndex`
- Produces:
  - `paths::AliceHome::{from_env() -> AliceHome, new(root: PathBuf) -> AliceHome, root(&self) -> &Path, db(&self) -> PathBuf, vector_file(&self, model: &str) -> PathBuf, models_dir, spool_dir, backups_dir, logs_dir, config_file, daemon_json, lock_file, token_file (all -> PathBuf), ensure_dirs(&self) -> std::io::Result<()>}`
  - `writer::{WriteOp, ProjectChoice { Slug(String), Detected(ProjectRef) }, WriteOutcome { Remembered(InsertOutcome, ResolvedProject), Ingested(IngestOutcome), Updated(Memory), Fact(FactOutcome), Done }, WriterHandle::{submit(&self, op) -> impl Future<Output = Result<WriteOutcome>>, submit_blocking(&self, op) -> Result<WriteOutcome>, send_nowait(&self, op)}, spawn_writer(conn: Connection, device: String, wake_worker: crossbeam_channel::Sender<()>) -> (WriterHandle, std::thread::JoinHandle<()>), ReadOnlyState::{reason(&self) -> Option<String>}}`
  - `WriteOp` variants: `Remember { input: NewMemoryInput, project: ProjectChoice }`, `Ingest { project: ProjectChoice, source: SourceInput, chunks: Vec<ChunkInput> }`, `Update { id: String, update: MemoryUpdate }`, `AssertFact(AssertFact)`, `StoreEmbeddings { model: String, items: Vec<(VecKey, Vec<f32>)> }`, `EmbedFailed { keys: Vec<VecKey>, retry: bool }`, `TouchAccess(Vec<i64>)`, `Shutdown`
  - `worker::spawn_worker(cfg: WorkerConfig) -> std::thread::JoinHandle<()>` with `WorkerConfig { readers: Arc<ReaderPool>, writer: WriterHandle, index: Arc<IndexSlot>, embedder: Arc<dyn Embedder>, wake: crossbeam_channel::Receiver<()>, stop: Arc<AtomicBool>, vector_path: PathBuf, batch: usize }`
  - `service::IndexSlot` = `parking_lot::RwLock<Option<Arc<VectorIndex>>>` (None while rebuilding)
  - `service::{ServiceOptions { relevance_floor: f32 (0.82), raw_min_results: usize (3), weights: Weights, reader_pool: usize (8) }, Service::{start(home: AliceHome, make_embedder: &dyn Fn() -> alice_core::Result<Arc<dyn Embedder>>, opts: ServiceOptions) -> alice_core::Result<Arc<Service>>, remember(&self, RememberRequest) -> Result<RememberResponse>, recall(&self, RecallRequest) -> Result<RecallResponse>, context(&self, ContextRequest) -> Result<ContextResponse>, get(&self, GetRequest) -> Result<GetResponse>, update(&self, UpdateRequest) -> Result<MemoryFull>, facts(&self, FactsRequest) -> Result<FactsResponse>, assert_fact(&self, AssertFactRequest) -> Result<FactOutcome>, ingest(&self, IngestRequest) -> Result<IngestOutcome>, health(&self) -> Result<Health>, projects(&self) -> Result<Vec<ResolvedProject>>, instance(&self) -> &str, shutdown(&self)}}` — all request methods are `async`.

- [ ] **Step 1: Crate manifest**

Workspace additions:
```toml
tokio = { version = "1.53.2", features = ["rt-multi-thread", "macros", "sync", "time", "signal", "net", "fs", "io-util"] }
crossbeam-channel = "0.5.17"
dirs = "7.0.0"
```

`crates/alice-daemon/Cargo.toml`:
```toml
[package]
name = "alice-daemon"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
rust-version.workspace = true
description = "Alice daemon: single-writer store, workers, HTTP API and MCP server"

[dependencies]
alice-core = { path = "../alice-core" }
tokio.workspace = true
crossbeam-channel.workspace = true
parking_lot.workspace = true
rusqlite.workspace = true
serde.workspace = true
serde_json.workspace = true
chrono.workspace = true
tracing.workspace = true
dirs.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

- [ ] **Step 2: `paths.rs`**

```rust
//! Filesystem layout of ALICE_HOME (spec §2.3).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AliceHome {
    root: PathBuf,
}

impl AliceHome {
    /// `ALICE_HOME` if set, otherwise `~/.alice`.
    pub fn from_env() -> Self {
        let root = std::env::var_os("ALICE_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".alice")))
            .unwrap_or_else(|| PathBuf::from(".alice"));
        Self { root }
    }

    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn db(&self) -> PathBuf {
        self.root.join("alice.db")
    }
    pub fn vector_file(&self, model: &str) -> PathBuf {
        self.root.join("vectors").join(format!("{model}.usearch"))
    }
    pub fn models_dir(&self) -> PathBuf {
        self.root.join("models")
    }
    pub fn spool_dir(&self) -> PathBuf {
        self.root.join("spool")
    }
    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }
    pub fn daemon_json(&self) -> PathBuf {
        self.root.join("daemon.json")
    }
    pub fn lock_file(&self) -> PathBuf {
        self.root.join("daemon.lock")
    }
    pub fn token_file(&self) -> PathBuf {
        self.root.join("token")
    }

    /// Create the directory tree. On Unix the root is private (0700).
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for d in [self.root.clone(), self.root.join("vectors"), self.models_dir(), self.spool_dir(), self.backups_dir(), self.logs_dir()] {
            std::fs::create_dir_all(&d)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}
```

- [ ] **Step 3: Failing tests for the writer (`writer.rs`)**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alice_core::store::memories::NewMemoryInput;

    fn writer() -> (tempfile::TempDir, WriterHandle, crossbeam_channel::Receiver<()>) {
        let d = tempfile::tempdir().unwrap();
        let conn = alice_core::store::open_writer(&d.path().join("a.db")).unwrap();
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded();
        let (h, _j) = spawn_writer(conn, "dev".into(), wake_tx);
        (d, h, wake_rx)
    }

    fn remember(text: &str) -> WriteOp {
        WriteOp::Remember { input: NewMemoryInput { content: text.into(), ..Default::default() }, project: ProjectChoice::Slug("global".into()) }
    }

    #[tokio::test]
    async fn one_bad_op_does_not_poison_its_batch() {
        let (_d, h, wake) = writer();
        let ok = h.submit(remember("good one"));
        let bad = h.submit(remember("   "));
        let ok2 = h.submit(remember("good two"));
        let (a, b, c) = tokio::join!(ok, bad, ok2);
        assert!(matches!(a.unwrap(), WriteOutcome::Remembered(InsertOutcome::New { .. }, _)));
        assert!(b.is_err());
        assert!(matches!(c.unwrap(), WriteOutcome::Remembered(InsertOutcome::New { .. }, _)));
        assert!(wake.try_recv().is_ok(), "worker is woken after inserts");
    }

    #[tokio::test]
    async fn unknown_project_slug_is_an_error_not_a_new_project() {
        let (_d, h, _w) = writer();
        let err = h
            .submit(WriteOp::Remember { input: NewMemoryInput { content: "x".into(), ..Default::default() }, project: ProjectChoice::Slug("nope".into()) })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown project"), "{err}");
    }

    #[test]
    fn blocking_submit_works_outside_the_runtime() {
        let (_d, h, _w) = writer();
        assert!(h.submit_blocking(remember("from a plain thread")).is_ok());
    }
}
```

- [ ] **Step 4: Implement `writer.rs`**

```rust
//! The single writer. Jobs arrive on a channel, are grouped for up to 2 ms (max 256),
//! and run in one transaction with a savepoint per job, so one failing job never rolls back the others.

use alice_core::error::{AliceError, Result};
use alice_core::ids::Hlc;
use alice_core::model::{Memory, Status};
use alice_core::project::ProjectRef;
use alice_core::store::embeddings::{self, VecKey};
use alice_core::store::kg::{self, AssertFact, EntityMatcher, FactOutcome};
use alice_core::store::memories::{self, InsertOutcome, MemoryUpdate, NewMemoryInput};
use alice_core::store::projects::{self, ResolvedProject};
use alice_core::store::sources::{self, ChunkInput, IngestOutcome, SourceInput};
use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use rusqlite::{Connection, ErrorCode};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

const BATCH_WINDOW: Duration = Duration::from_millis(2);
const BATCH_MAX: usize = 256;

#[derive(Debug, Clone)]
pub enum ProjectChoice {
    /// An existing slug or alias. Unknown slugs are rejected so agents cannot create stray projects.
    Slug(String),
    /// Detected from a working directory; created on first write.
    Detected(ProjectRef),
}

#[derive(Debug)]
pub enum WriteOp {
    Remember { input: NewMemoryInput, project: ProjectChoice },
    Ingest { project: ProjectChoice, source: SourceInput, chunks: Vec<ChunkInput> },
    Update { id: String, update: MemoryUpdate },
    AssertFact(AssertFact),
    StoreEmbeddings { model: String, items: Vec<(VecKey, Vec<f32>)> },
    EmbedFailed { keys: Vec<VecKey>, retry: bool },
    TouchAccess(Vec<i64>),
    Shutdown,
}

#[derive(Debug)]
pub enum WriteOutcome {
    Remembered(InsertOutcome, ResolvedProject),
    Ingested(IngestOutcome),
    Updated(Memory),
    Fact(FactOutcome),
    Done,
}

struct Job {
    op: WriteOp,
    reply: Option<oneshot::Sender<Result<WriteOutcome>>>,
}

/// Set when the database refuses writes (disk full, corruption); cleared only by restart after repair.
#[derive(Debug, Default)]
pub struct ReadOnlyState(Mutex<Option<String>>);

impl ReadOnlyState {
    pub fn reason(&self) -> Option<String> {
        self.0.lock().clone()
    }
    fn set(&self, reason: String) {
        *self.0.lock() = Some(reason);
    }
}

#[derive(Clone)]
pub struct WriterHandle {
    tx: Sender<Job>,
    pub read_only: Arc<ReadOnlyState>,
}

impl WriterHandle {
    pub async fn submit(&self, op: WriteOp) -> Result<WriteOutcome> {
        if let Some(r) = self.read_only.reason() {
            return Err(AliceError::ReadOnly(r));
        }
        let (tx, rx) = oneshot::channel();
        self.tx.send(Job { op, reply: Some(tx) }).map_err(|_| AliceError::Other("writer stopped; restart the daemon".into()))?;
        rx.await.map_err(|_| AliceError::Other("writer dropped the request; retry".into()))?
    }

    /// For non-async threads (the embedding worker).
    pub fn submit_blocking(&self, op: WriteOp) -> Result<WriteOutcome> {
        if let Some(r) = self.read_only.reason() {
            return Err(AliceError::ReadOnly(r));
        }
        let (tx, rx) = oneshot::channel();
        self.tx.send(Job { op, reply: Some(tx) }).map_err(|_| AliceError::Other("writer stopped".into()))?;
        rx.blocking_recv().map_err(|_| AliceError::Other("writer dropped the request".into()))?
    }

    /// Fire and forget (access counters).
    pub fn send_nowait(&self, op: WriteOp) {
        let _ = self.tx.send(Job { op, reply: None });
    }
}

fn resolve(conn: &Connection, choice: &ProjectChoice) -> Result<ResolvedProject> {
    match choice {
        ProjectChoice::Slug(s) => projects::resolve_slug(conn, s)?.ok_or_else(|| {
            AliceError::invalid(format!(
                "unknown project \"{s}\"; omit project to use the current one, or pass an existing project slug"
            ))
        }),
        ProjectChoice::Detected(p) => projects::upsert_project(conn, p),
    }
}

struct State {
    hlc: Hlc,
    matcher: Option<EntityMatcher>,
    inserted: bool,
}

fn apply(conn: &Connection, op: WriteOp, st: &mut State) -> Result<WriteOutcome> {
    match op {
        WriteOp::Remember { input, project } => {
            let p = resolve(conn, &project)?;
            let prepared = memories::prepare(input, &p, st.hlc.tick())?;
            let out = memories::insert(conn, &prepared)?;
            if matches!(out, InsertOutcome::New { .. }) {
                st.inserted = true;
            }
            Ok(WriteOutcome::Remembered(out, p))
        }
        WriteOp::Ingest { project, source, chunks } => {
            let p = resolve(conn, &project)?;
            let out = sources::ingest(conn, &p, &source, &chunks)?;
            if out.inserted > 0 {
                st.inserted = true;
            }
            Ok(WriteOutcome::Ingested(out))
        }
        WriteOp::Update { id, update } => {
            let m = memories::apply_update(conn, &id, &update, st.hlc.tick())?;
            st.inserted = true; // edits may need re-embedding
            Ok(WriteOutcome::Updated(m))
        }
        WriteOp::AssertFact(f) => {
            st.matcher = None; // entity list changed
            Ok(WriteOutcome::Fact(kg::assert_fact(conn, &f, st.hlc.tick())?))
        }
        WriteOp::StoreEmbeddings { model, items } => {
            if st.matcher.is_none() {
                st.matcher = Some(EntityMatcher::load(conn)?);
            }
            for (key, v) in &items {
                embeddings::put(conn, *key, &model, v)?;
                embeddings::mark_done(conn, *key)?;
                if let VecKey::Memory(seq) = key {
                    activate(conn, *seq, st.matcher.as_ref())?;
                }
            }
            Ok(WriteOutcome::Done)
        }
        WriteOp::EmbedFailed { keys, retry } => {
            for key in keys {
                embeddings::mark_failed(conn, key, retry)?;
                if let (VecKey::Memory(seq), false) = (key, retry) {
                    activate(conn, seq, None)?;
                }
            }
            Ok(WriteOutcome::Done)
        }
        WriteOp::TouchAccess(seqs) => {
            memories::touch_access(conn, &seqs)?;
            Ok(WriteOutcome::Done)
        }
        WriteOp::Shutdown => Ok(WriteOutcome::Done),
    }
}

/// Pending -> active, and record entity mentions.
fn activate(conn: &Connection, seq: i64, matcher: Option<&EntityMatcher>) -> Result<()> {
    let (id, title, body, status): (String, String, String, String) = conn.query_row(
        "SELECT id, title, body, status FROM memories WHERE seq = ?1",
        [seq],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    if status == Status::Pending.as_str() {
        memories::set_status_by_seq(conn, seq, Status::Active)?;
    }
    if let Some(m) = matcher {
        let ids = m.find_in(&format!("{title}\n{body}"));
        kg::link_mentions(conn, &id, &ids)?;
    }
    Ok(())
}

fn is_fatal(e: &rusqlite::Error) -> Option<String> {
    match e.sqlite_error_code() {
        Some(ErrorCode::DiskFull) => Some("the disk is full".into()),
        Some(ErrorCode::DatabaseCorrupt) | Some(ErrorCode::NotADatabase) => Some("the database file is damaged".into()),
        Some(ErrorCode::ReadOnly) | Some(ErrorCode::CannotOpen) => Some("the database cannot be written".into()),
        _ => None,
    }
}

pub fn spawn_writer(mut conn: Connection, device: String, wake_worker: Sender<()>) -> (WriterHandle, JoinHandle<()>) {
    let (tx, rx): (Sender<Job>, Receiver<Job>) = unbounded();
    let read_only = Arc::new(ReadOnlyState::default());
    let ro = read_only.clone();
    let handle = std::thread::Builder::new()
        .name("alice-writer".into())
        .spawn(move || {
            let mut st = State { hlc: Hlc::new(device), matcher: None, inserted: false };
            while let Ok(first) = rx.recv() {
                let stop = matches!(first.op, WriteOp::Shutdown);
                let mut batch = vec![first];
                let deadline = Instant::now() + BATCH_WINDOW;
                while !stop && batch.len() < BATCH_MAX {
                    match rx.recv_deadline(deadline) {
                        Ok(j) => batch.push(j),
                        Err(_) => break,
                    }
                }
                st.inserted = false;
                let mut replies = Vec::with_capacity(batch.len());
                let commit = (|| -> rusqlite::Result<()> {
                    let mut tx = conn.transaction()?;
                    for job in batch {
                        let sp = tx.savepoint()?;
                        let r = apply(&sp, job.op, &mut st);
                        if r.is_ok() {
                            sp.commit()?;
                        }
                        replies.push((job.reply, r));
                    }
                    tx.commit()
                })();
                match commit {
                    Ok(()) => {
                        for (reply, r) in replies {
                            if let Some(reply) = reply {
                                let _ = reply.send(r);
                            }
                        }
                        if st.inserted {
                            let _ = wake_worker.try_send(());
                        }
                    }
                    Err(e) => {
                        if let Some(reason) = is_fatal(&e) {
                            tracing::error!(%reason, "writer entering read-only mode");
                            ro.set(reason);
                        }
                        for (reply, _) in replies {
                            if let Some(reply) = reply {
                                let _ = reply.send(Err(AliceError::Other(format!("write failed: {e}; retry shortly"))));
                            }
                        }
                    }
                }
                if stop {
                    break;
                }
            }
        })
        .expect("spawn writer thread");
    (WriterHandle { tx, read_only }, handle)
}
```

> `apply` receives `&sp` (a `Savepoint`), which derefs to `Connection`. Errors inside `apply` drop the savepoint without committing, which rolls back only that job. The worker wake channel should be created with `crossbeam_channel::bounded(1)` by the caller so repeated wakes coalesce; `try_send` ignores a full channel.

- [ ] **Step 5: Implement `worker.rs`**

```rust
//! Embedding + enrichment worker. Reads pending items, embeds them off the writer thread,
//! hands vectors to the writer, then updates the in-memory HNSW index.

use crate::service::IndexSlot;
use crate::writer::{WriteOp, WriterHandle};
use alice_core::embed::Embedder;
use alice_core::store::ReaderPool;
use alice_core::store::embeddings::{self, VecKey};
use crossbeam_channel::Receiver;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const MAX_ATTEMPTS: u32 = 5;
const SAVE_EVERY: Duration = Duration::from_secs(30);

pub struct WorkerConfig {
    pub readers: Arc<ReaderPool>,
    pub writer: WriterHandle,
    pub index: Arc<IndexSlot>,
    pub embedder: Arc<dyn Embedder>,
    pub wake: Receiver<()>,
    pub stop: Arc<AtomicBool>,
    pub vector_path: PathBuf,
    pub batch: usize,
}

pub fn spawn_worker(cfg: WorkerConfig) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("alice-embed".into())
        .spawn(move || run(cfg))
        .expect("spawn worker thread")
}

fn run(cfg: WorkerConfig) {
    let mut attempts: HashMap<VecKey, u32> = HashMap::new();
    let mut backoff = Duration::from_millis(500);
    let mut last_save = Instant::now();
    loop {
        if cfg.stop.load(Ordering::Relaxed) {
            break;
        }
        let Some(index) = cfg.index.read().clone() else {
            // Index is being rebuilt; vectors would be lost if we inserted now.
            std::thread::sleep(Duration::from_millis(200));
            continue;
        };
        let items = match cfg.readers.with(|c| embeddings::pending(c, cfg.batch)) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, "reading embed queue failed");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        if items.is_empty() {
            if index.is_dirty() && last_save.elapsed() >= SAVE_EVERY {
                if let Err(e) = index.save(&cfg.vector_path) {
                    tracing::warn!(error = %e, "saving vector index failed");
                }
                last_save = Instant::now();
            }
            let _ = cfg.wake.recv_timeout(Duration::from_secs(1));
            continue;
        }
        let texts: Vec<String> = items.iter().map(|i| i.text.clone()).collect();
        match cfg.embedder.embed_passages(&texts) {
            Ok(vectors) => {
                backoff = Duration::from_millis(500);
                let pairs: Vec<(VecKey, Vec<f32>)> = items.iter().map(|i| i.key).zip(vectors).collect();
                match cfg.writer.submit_blocking(WriteOp::StoreEmbeddings { model: cfg.embedder.model_id().to_string(), items: pairs.clone() }) {
                    Ok(_) => {
                        for (key, v) in &pairs {
                            if let Err(e) = index.upsert(*key, v) {
                                tracing::warn!(error = %e, ?key, "index upsert failed; next consistency check rebuilds");
                            }
                            attempts.remove(key);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "storing embeddings failed");
                        std::thread::sleep(backoff);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "embedding failed; backing off");
                let mut retry = Vec::new();
                let mut give_up = Vec::new();
                for i in &items {
                    let n = attempts.entry(i.key).or_insert(0);
                    *n += 1;
                    if *n >= MAX_ATTEMPTS { give_up.push(i.key) } else { retry.push(i.key) }
                }
                if !retry.is_empty() {
                    let _ = cfg.writer.submit_blocking(WriteOp::EmbedFailed { keys: retry, retry: true });
                }
                if !give_up.is_empty() {
                    let _ = cfg.writer.submit_blocking(WriteOp::EmbedFailed { keys: give_up, retry: false });
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
    if let Some(index) = cfg.index.read().clone() {
        let _ = index.save(&cfg.vector_path);
    }
}
```

- [ ] **Step 6: Failing service tests (`service.rs`)**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alice_core::api::*;
    use alice_core::embed::FakeEmbedder;
    use alice_core::model::Kind;

    async fn svc() -> (tempfile::TempDir, Arc<Service>) {
        let d = tempfile::tempdir().unwrap();
        let s = Service::start(AliceHome::new(d.path().to_path_buf()), &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>), ServiceOptions::default()).unwrap();
        (d, s)
    }

    async fn wait_embedded(s: &Service) {
        for _ in 0..200 {
            if s.health().await.unwrap().embed_backlog == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("embedding backlog did not drain");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remember_recall_get_update_round_trip() {
        let (d, s) = svc().await;
        let repo = d.path().join("proj");
        std::fs::create_dir(&repo).unwrap();
        let cwd = Some(repo.to_string_lossy().into_owned());
        let r = s.remember(RememberRequest { content: "Use WAL with one writer thread".into(), kind: Some(Kind::Decision), cwd: cwd.clone(), ..Default::default() }).await.unwrap();
        assert_eq!(r.status, RememberStatus::New);
        assert_eq!(r.project, "proj");
        let dup = s.remember(RememberRequest { content: "Use WAL with one writer thread".into(), kind: Some(Kind::Decision), cwd: cwd.clone(), ..Default::default() }).await.unwrap();
        assert_eq!(dup.status, RememberStatus::Duplicate);
        wait_embedded(&s).await;
        let found = s.recall(RecallRequest { query: "writer thread".into(), cwd: cwd.clone(), format: OutputFormat::Json, ..Default::default() }).await.unwrap();
        assert_eq!(found.results[0].id, r.id.clone().unwrap());
        let text = s.recall(RecallRequest { query: "writer thread".into(), cwd: cwd.clone(), ..Default::default() }).await.unwrap().text.unwrap();
        assert!(text.contains("Use WAL"));
        let got = s.get(GetRequest { ids: vec![r.id.clone().unwrap(), "MISSING".into()], include_provenance: false }).await.unwrap();
        assert_eq!(got.memories.len(), 1);
        assert_eq!(got.missing, vec!["MISSING".to_string()]);
        assert_eq!(got.memories[0].memory.status, alice_core::model::Status::Active);
        let ctx = s.context(ContextRequest { cwd: cwd.clone(), ..Default::default() }).await.unwrap();
        assert!(ctx.text.contains("Use WAL"), "{}", ctx.text);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn facts_round_trip_and_unknown_project_errors() {
        let (_d, s) = svc().await;
        s.assert_fact(AssertFactRequest { subject: "alice".into(), predicate: "written in".into(), object: "Rust".into(), ..Default::default() }).await.unwrap();
        let f = s.facts(FactsRequest { entity: "Alice".into(), ..Default::default() }).await.unwrap();
        assert_eq!(f.facts[0].predicate, "written_in");
        assert!(f.text.contains("written_in"));
        let err = s.remember(RememberRequest { content: "x".into(), project: Some("ghost".into()), ..Default::default() }).await.unwrap_err();
        assert!(err.to_string().contains("unknown project"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restart_keeps_data_and_rebuilds_a_missing_index() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let mk = || Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>);
        let s = Service::start(home.clone(), &mk, ServiceOptions::default()).unwrap();
        let id = s.remember(RememberRequest { content: "persist me across restarts".into(), ..Default::default() }).await.unwrap().id.unwrap();
        wait_embedded(&s).await;
        s.shutdown();
        drop(s);
        let _ = std::fs::remove_file(home.vector_file("fake"));
        let s2 = Service::start(home, &mk, ServiceOptions::default()).unwrap();
        for _ in 0..100 {
            if s2.health().await.unwrap().index_ready { break; }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let out = s2.recall(RecallRequest { query: "persist restarts".into(), format: OutputFormat::Json, ..Default::default() }).await.unwrap();
        assert_eq!(out.results[0].id, id);
    }
}
```

- [ ] **Step 7: Implement `service.rs`**

```rust
//! The service layer: every operation, used identically by the HTTP API, MCP and CLI.

use crate::paths::AliceHome;
use crate::worker::{WorkerConfig, spawn_worker};
use crate::writer::{ProjectChoice, WriteOp, WriteOutcome, WriterHandle, spawn_writer};
use alice_core::api::*;
use alice_core::embed::Embedder;
use alice_core::error::{AliceError, Result};
use alice_core::ids::new_id;
use alice_core::model::limits;
use alice_core::normalize::normalize_topic;
use alice_core::project::{ProjectRef, detect_project};
use alice_core::search::{self, SearchParams, fusion::Weights, pack};
use alice_core::store::embeddings;
use alice_core::store::kg::{self, FactOutcome};
use alice_core::store::memories::{self, InsertOutcome, NewMemoryInput};
use alice_core::store::projects::{self, ResolvedProject};
use alice_core::store::sources::{self, IngestOutcome};
use alice_core::store::{ReaderPool, open_reader, open_writer};
use alice_core::vector::VectorIndex;
use parking_lot::{Mutex, RwLock};
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// `None` while the index is being rebuilt; searches then use lexical candidates only.
pub type IndexSlot = RwLock<Option<Arc<VectorIndex>>>;

#[derive(Debug, Clone)]
pub struct ServiceOptions {
    pub relevance_floor: f32,
    pub raw_min_results: usize,
    pub weights: Weights,
    pub reader_pool: usize,
}

impl Default for ServiceOptions {
    fn default() -> Self {
        Self { relevance_floor: 0.82, raw_min_results: 3, weights: Weights::default(), reader_pool: 8 }
    }
}

/// Maps (explicit project, working directory) to a project. Cheap to clone; caches git lookups for 60 s.
#[derive(Clone, Default)]
pub struct ProjectResolver {
    cache: Arc<Mutex<HashMap<String, (ProjectRef, Instant)>>>,
}

impl ProjectResolver {
    fn detect(&self, cwd: &str) -> ProjectRef {
        if let Some((p, at)) = self.cache.lock().get(cwd) {
            if at.elapsed() < Duration::from_secs(60) {
                return p.clone();
            }
        }
        let p = detect_project(Path::new(cwd));
        self.cache.lock().insert(cwd.to_string(), (p.clone(), Instant::now()));
        p
    }

    /// Write side: an explicit slug must exist; otherwise the working directory decides; otherwise `global`.
    pub fn choose(&self, explicit: Option<&str>, cwd: Option<&str>) -> ProjectChoice {
        match (explicit.map(str::trim).filter(|s| !s.is_empty()), cwd) {
            (Some(s), _) => ProjectChoice::Slug(s.to_string()),
            (None, Some(c)) => ProjectChoice::Detected(self.detect(c)),
            (None, None) => ProjectChoice::Slug("global".into()),
        }
    }

    /// Read side: (filter project id, current project id, display slug). Never creates projects.
    pub fn read_project(&self, conn: &Connection, explicit: Option<&str>, cwd: Option<&str>) -> Result<(Option<String>, Option<String>, String)> {
        let is_explicit = explicit.is_some_and(|s| !s.trim().is_empty());
        match self.choose(explicit, cwd) {
            ProjectChoice::Slug(s) => {
                let p = projects::resolve_slug(conn, &s)?.ok_or_else(|| {
                    AliceError::invalid(format!("unknown project \"{s}\"; omit project or use an existing slug"))
                })?;
                Ok((is_explicit.then(|| p.id.clone()), Some(p.id), p.slug))
            }
            ProjectChoice::Detected(r) => {
                let found = projects::find_by_key(conn, &r.key)?;
                Ok((None, found.as_ref().map(|p| p.id.clone()), found.map(|p| p.slug).unwrap_or(r.slug)))
            }
        }
    }
}

pub struct Service {
    home: AliceHome,
    readers: Arc<ReaderPool>,
    writer: WriterHandle,
    index: Arc<IndexSlot>,
    query_embedder: Arc<dyn Embedder>,
    opts: ServiceOptions,
    instance: String,
    stop: Arc<AtomicBool>,
    resolver: ProjectResolver,
}

fn device_id(conn: &Connection) -> Result<String> {
    if let Some(d) = conn.query_row("SELECT value FROM meta WHERE key = 'device_id'", [], |r| r.get::<_, String>(0)).optional()? {
        return Ok(d);
    }
    let d = new_id()[20..].to_lowercase();
    conn.execute("INSERT INTO meta (key, value) VALUES ('device_id', ?1)", [&d])?;
    Ok(d)
}

fn join_err(e: tokio::task::JoinError) -> AliceError {
    AliceError::Other(format!("internal task failed: {e}"))
}

fn unexpected(o: WriteOutcome) -> AliceError {
    AliceError::Other(format!("unexpected writer outcome {o:?}"))
}

impl Service {
    pub fn start(home: AliceHome, make_embedder: &dyn Fn() -> Result<Arc<dyn Embedder>>, opts: ServiceOptions) -> Result<Arc<Service>> {
        home.ensure_dirs()?;
        let conn = open_writer(&home.db())?;
        let device = device_id(&conn)?;
        let readers = Arc::new(ReaderPool::new(home.db(), opts.reader_pool));
        // Two instances: batch embedding in the worker must never delay a query embedding.
        let worker_embedder = make_embedder()?;
        let query_embedder = make_embedder()?;
        let model = worker_embedder.model_id().to_string();
        let dim = worker_embedder.dim();
        let index: Arc<IndexSlot> = Arc::new(RwLock::new(None));
        let vector_path = home.vector_file(&model);

        let expected = embeddings::count(&conn, &model)?;
        match VectorIndex::open(&vector_path, dim) {
            Ok(idx) if idx.len() == expected => *index.write() = Some(Arc::new(idx)),
            _ => {
                let slot = index.clone();
                let db = home.db();
                let model = model.clone();
                std::thread::Builder::new().name("alice-index-rebuild".into()).spawn(move || {
                    let started = Instant::now();
                    match open_reader(&db).and_then(|c| VectorIndex::rebuild_from(&c, &model, dim)) {
                        Ok(idx) => {
                            tracing::info!(n = idx.len(), secs = started.elapsed().as_secs_f32(), "vector index rebuilt");
                            *slot.write() = Some(Arc::new(idx));
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "vector index rebuild failed; starting with an empty index");
                            *slot.write() = VectorIndex::new(dim).ok().map(Arc::new);
                        }
                    }
                })?;
            }
        }

        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        let (writer, _writer_thread) = spawn_writer(conn, device, wake_tx);
        let stop = Arc::new(AtomicBool::new(false));
        spawn_worker(WorkerConfig {
            readers: readers.clone(),
            writer: writer.clone(),
            index: index.clone(),
            embedder: worker_embedder,
            wake: wake_rx,
            stop: stop.clone(),
            vector_path,
            batch: 32,
        });
        Ok(Arc::new(Service {
            home,
            readers,
            writer,
            index,
            query_embedder,
            opts,
            instance: new_id(),
            stop,
            resolver: ProjectResolver::default(),
        }))
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    pub fn home(&self) -> &AliceHome {
        &self.home
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.writer.send_nowait(WriteOp::Shutdown);
        if let Some(idx) = self.index.read().clone() {
            let _ = idx.save(&self.home.vector_file(self.query_embedder.model_id()));
        }
    }

    async fn read<T: Send + 'static>(&self, f: impl FnOnce(&Connection) -> Result<T> + Send + 'static) -> Result<T> {
        let readers = self.readers.clone();
        tokio::task::spawn_blocking(move || readers.with(f)).await.map_err(join_err)?
    }

    pub async fn remember(&self, req: RememberRequest) -> Result<RememberResponse> {
        let project = self.resolver.choose(req.project.as_deref(), req.cwd.as_deref());
        let input = NewMemoryInput {
            content: req.content,
            kind: req.kind,
            title: req.title,
            topics: req.topics,
            importance: req.importance,
            supersedes: req.supersedes,
            idempotency_key: req.idempotency_key,
            author: req.author,
            ..Default::default()
        };
        match self.writer.submit(WriteOp::Remember { input, project }).await? {
            WriteOutcome::Remembered(InsertOutcome::New { id, .. }, p) => Ok(RememberResponse { id: Some(id), status: RememberStatus::New, project: p.slug }),
            WriteOutcome::Remembered(InsertOutcome::Duplicate { id }, p) => Ok(RememberResponse { id: Some(id), status: RememberStatus::Duplicate, project: p.slug }),
            other => Err(unexpected(other)),
        }
    }

    pub async fn recall(&self, req: RecallRequest) -> Result<RecallResponse> {
        let limit = req.limit.unwrap_or(limits::DEFAULT_RECALL_LIMIT).min(limits::MAX_RECALL_LIMIT);
        let max_tokens = req.max_tokens.unwrap_or(limits::DEFAULT_RECALL_TOKENS);
        let format = req.format;
        let opts = self.opts.clone();
        let index = self.index.read().clone();
        let embedder = self.query_embedder.clone();
        let resolver = self.resolver.clone();
        let (out, slug) = self
            .read(move |c| {
                let (filter, current, slug) = resolver.read_project(c, req.project.as_deref(), req.cwd.as_deref())?;
                let params = SearchParams {
                    query: req.query,
                    project_filter_id: filter,
                    current_project_id: current,
                    topics: req.topics.iter().filter_map(|t| normalize_topic(t)).collect(),
                    kinds: req.kinds,
                    since: req.since,
                    before: req.before,
                    include_raw: req.include_raw,
                    limit,
                    relevance_floor: opts.relevance_floor,
                    raw_min_results: opts.raw_min_results,
                };
                let out = search::run(c, index.as_deref(), embedder.as_ref(), &opts.weights, &params, chrono::Utc::now())?;
                Ok((out, slug))
            })
            .await?;
        if !out.memory_seqs.is_empty() {
            self.writer.send_nowait(WriteOp::TouchAccess(out.memory_seqs.clone()));
        }
        Ok(match format {
            OutputFormat::Text => {
                let (text, omitted) = pack::pack_text(&out, Some(&slug), max_tokens);
                RecallResponse { project: Some(slug), omitted, raw_fallback: out.raw_fallback, text: Some(text), ..Default::default() }
            }
            OutputFormat::Json => pack::pack_json(out, Some(slug), max_tokens),
        })
    }

    pub async fn context(&self, req: ContextRequest) -> Result<ContextResponse> {
        let max_tokens = req.max_tokens.unwrap_or(limits::DEFAULT_CONTEXT_TOKENS);
        let resolver = self.resolver.clone();
        self.read(move |c| {
            let (_, current, slug) = resolver.read_project(c, req.project.as_deref(), req.cwd.as_deref())?;
            let text = match current {
                Some(id) => alice_core::context::build(c, &projects::get_by_id(c, &id)?, max_tokens, chrono::Utc::now())?,
                None => format!(
                    "# Context · {slug}\nNo memories yet for {slug}. Save decisions, fixes and lessons with remember; use recall to search everything.\n"
                ),
            };
            Ok(ContextResponse { project: slug, text })
        })
        .await
    }

    pub async fn get(&self, req: GetRequest) -> Result<GetResponse> {
        self.read(move |c| {
            let found = memories::get_many(c, &req.ids)?;
            let mut out = GetResponse::default();
            for id in &req.ids {
                if !found.iter().any(|m| &m.id == id || m.legacy_id.as_deref() == Some(id.as_str())) {
                    out.missing.push(id.clone());
                }
            }
            for m in found {
                out.memories.push(full(c, m, req.include_provenance)?);
            }
            Ok(out)
        })
        .await
    }

    pub async fn update(&self, req: UpdateRequest) -> Result<MemoryFull> {
        let m = match self.writer.submit(WriteOp::Update { id: req.id, update: req.update }).await? {
            WriteOutcome::Updated(m) => m,
            other => return Err(unexpected(other)),
        };
        self.read(move |c| full(c, m, false)).await
    }

    pub async fn facts(&self, req: FactsRequest) -> Result<FactsResponse> {
        self.read(move |c| {
            let facts = kg::query_facts(c, &req)?;
            let mut text = String::new();
            if facts.is_empty() {
                text.push_str(&format!("No facts about \"{}\".\n", req.entity));
            }
            for f in &facts {
                let day = |t: &str| t.chars().take(10).collect::<String>();
                let until = f.valid_to.as_deref().map(|t| format!(" until {}", day(t))).unwrap_or_default();
                text.push_str(&format!("- {} {} {} (since {}{}) [{}]\n", f.subject, f.predicate, f.object, day(&f.valid_from), until, f.id));
            }
            Ok(FactsResponse { entity: req.entity.clone(), facts, text })
        })
        .await
    }

    pub async fn assert_fact(&self, req: AssertFactRequest) -> Result<FactOutcome> {
        match self.writer.submit(WriteOp::AssertFact(req)).await? {
            WriteOutcome::Fact(f) => Ok(f),
            other => Err(unexpected(other)),
        }
    }

    pub async fn ingest(&self, req: IngestRequest) -> Result<IngestOutcome> {
        sources::validate_chunks(&req.chunks)?;
        let project = self.resolver.choose(req.project.as_deref(), req.cwd.as_deref());
        match self.writer.submit(WriteOp::Ingest { project, source: req.source, chunks: req.chunks }).await? {
            WriteOutcome::Ingested(o) => Ok(o),
            other => Err(unexpected(other)),
        }
    }

    pub async fn projects(&self) -> Result<Vec<ResolvedProject>> {
        self.read(|c| projects::list_projects(c)).await
    }

    pub async fn health(&self) -> Result<Health> {
        let instance = self.instance.clone();
        let read_only = self.writer.read_only.reason();
        let index_ready = self.index.read().is_some();
        self.read(move |c| {
            let count = |sql: &str| -> Result<u64> { Ok(c.query_row(sql, [], |r| r.get::<_, i64>(0))? as u64) };
            Ok(Health {
                service: "alice".into(),
                instance,
                version: env!("CARGO_PKG_VERSION").into(),
                pid: std::process::id(),
                memories: count("SELECT count(*) FROM memories WHERE status <> 'trashed'")?,
                chunks: count("SELECT count(*) FROM chunks")?,
                projects: count("SELECT count(*) FROM projects")?,
                embed_backlog: embeddings::backlog(c)? as u64,
                read_only,
                index_ready,
            })
        })
        .await
    }
}

fn full(c: &Connection, m: alice_core::model::Memory, include_provenance: bool) -> Result<MemoryFull> {
    let mut links = Vec::new();
    let mut stmt = c.prepare_cached(
        "SELECT to_id, type, 'out' FROM links WHERE from_id = ?1 UNION ALL SELECT from_id, type, 'in' FROM links WHERE to_id = ?1",
    )?;
    for r in stmt.query_map([&m.id], |r| Ok(LinkRef { id: r.get(0)?, link_type: r.get(1)?, direction: r.get(2)? }))? {
        links.push(r?);
    }
    let provenance = if include_provenance {
        let mut stmt = c.prepare_cached("SELECT chunk_id FROM provenance WHERE memory_id = ?1")?;
        stmt.query_map([&m.id], |r| r.get(0))?.collect::<std::result::Result<Vec<String>, _>>()?
    } else {
        Vec::new()
    };
    Ok(MemoryFull { memory: m, links, provenance })
}
```

- [ ] **Step 8: Service-level concurrency regression test**

`crates/alice-daemon/tests/service_concurrency.rs`:
```rust
//! The MemPalace failure as a regression test: many clients writing and reading while a bulk ingest runs.

use alice_core::api::*;
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::store::sources::{ChunkInput, SourceInput};
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn p95(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[(v.len() as f64 * 0.95) as usize - 1]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn writes_and_reads_stay_fast_during_bulk_ingest() {
    let d = tempfile::tempdir().unwrap();
    let svc = Service::start(AliceHome::new(d.path().to_path_buf()), &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>), ServiceOptions::default()).unwrap();
    let chunks_total: usize = std::env::var("ALICE_TEST_BULK").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000);

    let bulk = {
        let svc = svc.clone();
        tokio::spawn(async move {
            for batch in 0..(chunks_total / 1_000) {
                let chunks = (0..1_000).map(|i| ChunkInput { ord: (batch * 1_000 + i) as i64, text: format!("transcript line {batch}-{i} about lease timeouts and sqlite"), ..Default::default() }).collect();
                svc.ingest(IngestRequest { source: SourceInput { uri: "bulk".into(), ..Default::default() }, chunks, ..Default::default() }).await.unwrap();
            }
        })
    };

    let mut clients = Vec::new();
    for c in 0..20 {
        let svc = svc.clone();
        clients.push(tokio::spawn(async move {
            let mut w = Vec::new();
            let mut r = Vec::new();
            let mut ids = Vec::new();
            for i in 0..25 {
                let t = Instant::now();
                let resp = svc.remember(RememberRequest { content: format!("client {c} decision {i}: keep the writer short"), ..Default::default() }).await.unwrap();
                w.push(t.elapsed());
                ids.push(resp.id.unwrap());
                let t = Instant::now();
                svc.recall(RecallRequest { query: format!("client {c} decision"), ..Default::default() }).await.unwrap();
                r.push(t.elapsed());
            }
            (w, r, ids)
        }));
    }
    let mut writes = Vec::new();
    let mut reads = Vec::new();
    let mut all_ids = Vec::new();
    for h in clients {
        let (w, r, ids) = h.await.unwrap();
        writes.extend(w);
        reads.extend(r);
        all_ids.extend(ids);
    }
    bulk.await.unwrap();

    let (wp, rp) = (p95(writes), p95(reads));
    let factor = if cfg!(debug_assertions) { 6 } else { 1 };
    assert!(wp < Duration::from_millis(50 * factor), "remember p95 {wp:?}");
    assert!(rp < Duration::from_millis(200 * factor), "recall p95 {rp:?}");
    let got = svc.get(GetRequest { ids: all_ids.clone(), include_provenance: false }).await.unwrap();
    assert_eq!(got.memories.len(), all_ids.len(), "no acknowledged write is lost");
    assert!(got.missing.is_empty());
}
```

- [ ] **Step 9: Run tests**

Run: `nice -n 15 cargo test -j 6 -p alice-daemon`
Then the release-mode budget check: `nice -n 15 cargo test -j 6 --release -p alice-daemon --test service_concurrency`
Expected: PASS in both. If p95 fails in release, profile before relaxing anything; the budgets come from the spec.

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml crates/alice-core crates/alice-daemon
git commit -m "feat(daemon): single writer with batched savepoints, embedding worker, background index rebuild and service layer

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 11: HTTP API, auth guard, spool and daemon lifecycle

**Files:**
- Create: `crates/alice-daemon/src/{http.rs,spool.rs,discovery.rs,daemon.rs}`
- Modify: `crates/alice-daemon/Cargo.toml`, `crates/alice-daemon/src/lib.rs`, `Cargo.toml` (workspace deps `axum`, `tower-http` not needed, `tokio-util`, `getrandom`)
- Test: `crates/alice-daemon/tests/http_api.rs`, unit tests in `spool.rs` and `discovery.rs`

**Interfaces:**
- Consumes: `service::Service` (Task 10), `paths::AliceHome`, `alice_core::api::*`, `alice_embed::{FastEmbedder, fetch_model, is_model_present, install_tls_provider}`
- Produces:
  - `discovery::{DaemonInfo { pid: u32, port: u16, instance: String, version: String, started_at: String }, read_info(home: &AliceHome) -> Option<DaemonInfo>, write_info(home: &AliceHome, info: &DaemonInfo) -> std::io::Result<()>, remove_info_if(home: &AliceHome, instance: &str), load_or_create_token(home: &AliceHome) -> std::io::Result<String>, read_token(home: &AliceHome) -> Option<String>}`
  - `spool::{SpoolEntry { Remember(RememberRequest), Ingest(IngestRequest) } (serde tag "op"), append(dir: &Path, entry: &SpoolEntry) -> std::io::Result<PathBuf>, DrainReport { processed: usize, rejected: usize, remaining: usize }, drain(dir: &Path, svc: &Service) -> DrainReport (async), pending_count(dir: &Path) -> usize}`
  - `http::{router(svc: Arc<Service>, token: String, port: u16, shutdown: CancellationToken) -> axum::Router, ApiError, PingResponse { service: String, instance: String, version: String }}` — routes: `GET /api/v1/ping` (Host check only, no token), and with token: `GET /api/v1/health`, `GET /api/v1/projects`, `POST /api/v1/{remember,recall,context,get,update,facts/query,facts/assert,ingest,admin/shutdown}`. Error body: `{"error": "<message>", "kind": "invalid|not_found|read_only|internal"}` with status 400/404/503/500.
  - `daemon::{DaemonOptions { port: u16 (0 = random), fake_embedder: bool }, run(home: AliceHome, opts: DaemonOptions) -> anyhow::Result<()>}` (blocking; owns its tokio runtime)
  - `alice_daemon::install_tls()` (re-export of `alice_embed::install_tls_provider`)

- [ ] **Step 1: Dependencies**

Workspace additions:
```toml
axum = "0.8.9"
tokio-util = "0.7.20"
getrandom = "0.4.3"
anyhow = "1.0.104"
```
`crates/alice-daemon/Cargo.toml` adds: `alice-embed = { path = "../alice-embed" }`, `axum.workspace = true`, `tokio-util.workspace = true`, `getrandom.workspace = true`, `anyhow.workspace = true`, `hex.workspace = true`, `reqwest.workspace = true`, `futures.workspace = true`; dev-dependencies keep `tempfile`.

`lib.rs`:
```rust
//! Alice daemon library: storage service, HTTP API, MCP server and client.

pub mod daemon;
pub mod discovery;
pub mod http;
pub mod paths;
pub mod service;
pub mod spool;
pub mod worker;
pub mod writer;

pub use alice_embed::install_tls_provider as install_tls;
```

- [ ] **Step 2: `discovery.rs` with tests**

```rust
//! `daemon.json` (how clients find the daemon) and the API token file.

use crate::paths::AliceHome;
use serde::{Deserialize, Serialize};
use std::io::Write;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub pid: u32,
    pub port: u16,
    /// Random per start. Clients compare it with `/api/v1/ping` before sending the token.
    pub instance: String,
    pub version: String,
    pub started_at: String,
}

pub fn read_info(home: &AliceHome) -> Option<DaemonInfo> {
    let raw = std::fs::read_to_string(home.daemon_json()).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Atomic: write a temp file, then rename.
pub fn write_info(home: &AliceHome, info: &DaemonInfo) -> std::io::Result<()> {
    let tmp = home.daemon_json().with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(info)?)?;
    std::fs::rename(tmp, home.daemon_json())
}

/// Remove daemon.json only if it still describes this instance (a newer daemon may have replaced it).
pub fn remove_info_if(home: &AliceHome, instance: &str) {
    if read_info(home).is_some_and(|i| i.instance == instance) {
        let _ = std::fs::remove_file(home.daemon_json());
    }
}

pub fn read_token(home: &AliceHome) -> Option<String> {
    std::fs::read_to_string(home.token_file()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// 32 random bytes as hex, stored with mode 0600. Reused across restarts.
pub fn load_or_create_token(home: &AliceHome) -> std::io::Result<String> {
    if let Some(t) = read_token(home) {
        return Ok(t);
    }
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| std::io::Error::other(e.to_string()))?;
    let token = hex::encode(buf);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(home.token_file()) {
        Ok(mut f) => {
            f.write_all(token.as_bytes())?;
            Ok(token)
        }
        // Another process created it first: use theirs.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read_token(home).ok_or(e),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_round_trip_and_conditional_remove() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        home.ensure_dirs().unwrap();
        let info = DaemonInfo { pid: 1, port: 2, instance: "a".into(), version: "v".into(), started_at: "t".into() };
        write_info(&home, &info).unwrap();
        assert_eq!(read_info(&home), Some(info));
        remove_info_if(&home, "b");
        assert!(read_info(&home).is_some());
        remove_info_if(&home, "a");
        assert!(read_info(&home).is_none());
    }

    #[test]
    fn token_is_stable_and_private() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        home.ensure_dirs().unwrap();
        let a = load_or_create_token(&home).unwrap();
        let b = load_or_create_token(&home).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.token_file()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
```

- [ ] **Step 3: `spool.rs` with tests**

```rust
//! Requests written to disk while the daemon is unavailable or read-only; drained in order.
//! One file per request (`<ulid>.json`), written atomically, so concurrent writers never collide.

use crate::service::Service;
use alice_core::api::{IngestRequest, RememberRequest};
use alice_core::error::AliceError;
use alice_core::ids::new_id;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SpoolEntry {
    Remember(RememberRequest),
    Ingest(IngestRequest),
}

pub fn append(dir: &Path, entry: &SpoolEntry) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let id = new_id();
    let tmp = dir.join(format!("{id}.tmp"));
    let dest = dir.join(format!("{id}.json"));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(&serde_json::to_vec(entry)?)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "json")).collect())
        .unwrap_or_default();
    files.sort();
    files
}

pub fn pending_count(dir: &Path) -> usize {
    entries(dir).len()
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DrainReport {
    pub processed: usize,
    pub rejected: usize,
    pub remaining: usize,
}

fn reject(dir: &Path, file: &Path, why: &str) {
    let rejected = dir.join("rejected");
    let _ = std::fs::create_dir_all(&rejected);
    if let Some(name) = file.file_name() {
        let _ = std::fs::rename(file, rejected.join(name));
        let _ = std::fs::write(rejected.join(name).with_extension("err"), why);
    }
}

/// Apply spooled requests oldest first. Invalid requests move to `spool/rejected/`;
/// a transient failure stops the drain so the order is kept for the next attempt.
pub async fn drain(dir: &Path, svc: &Service) -> DrainReport {
    let mut report = DrainReport::default();
    let files = entries(dir);
    for (i, file) in files.iter().enumerate() {
        let parsed: Result<SpoolEntry, String> = std::fs::read(file)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()));
        let entry = match parsed {
            Ok(e) => e,
            Err(why) => {
                reject(dir, file, &why);
                report.rejected += 1;
                continue;
            }
        };
        let result = match entry {
            SpoolEntry::Remember(r) => svc.remember(r).await.map(|_| ()),
            SpoolEntry::Ingest(r) => svc.ingest(r).await.map(|_| ()),
        };
        match result {
            Ok(()) => {
                let _ = std::fs::remove_file(file);
                report.processed += 1;
            }
            Err(e @ (AliceError::Invalid(_) | AliceError::NotFound(_))) => {
                reject(dir, file, &e.to_string());
                report.rejected += 1;
            }
            Err(e) => {
                tracing::warn!(error = %e, "spool drain paused");
                report.remaining = files.len() - i;
                return report;
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::AliceHome;
    use crate::service::ServiceOptions;
    use alice_core::embed::{Embedder, FakeEmbedder};
    use std::sync::Arc;

    #[tokio::test(flavor = "multi_thread")]
    async fn drain_applies_in_order_and_rejects_bad_entries() {
        let d = tempfile::tempdir().unwrap();
        let home = AliceHome::new(d.path().to_path_buf());
        let svc = Service::start(home.clone(), &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>), ServiceOptions::default()).unwrap();
        let dir = home.spool_dir();
        append(&dir, &SpoolEntry::Remember(RememberRequest { content: "spooled one".into(), ..Default::default() })).unwrap();
        append(&dir, &SpoolEntry::Remember(RememberRequest { content: "   ".into(), ..Default::default() })).unwrap();
        std::fs::write(dir.join("zzzz.json"), b"not json").unwrap();
        append(&dir, &SpoolEntry::Remember(RememberRequest { content: "spooled two".into(), ..Default::default() })).unwrap();
        let r = drain(&dir, &svc).await;
        assert_eq!(r, DrainReport { processed: 2, rejected: 2, remaining: 0 });
        assert_eq!(pending_count(&dir), 0);
        assert_eq!(std::fs::read_dir(dir.join("rejected")).unwrap().count(), 4, "two entries plus two .err notes");
    }
}
```

- [ ] **Step 4: Failing HTTP tests (`crates/alice-daemon/tests/http_api.rs`)**

```rust
use alice_core::api::*;
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_daemon::http::router;
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct Srv {
    _d: tempfile::TempDir,
    port: u16,
    token: String,
    http: reqwest::Client,
}

impl Srv {
    async fn start() -> Srv {
        alice_daemon::install_tls();
        let d = tempfile::tempdir().unwrap();
        let svc = Service::start(AliceHome::new(d.path().to_path_buf()), &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>), ServiceOptions::default()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = "t".repeat(64);
        let app = router(svc, token.clone(), port, CancellationToken::new());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Srv { _d: d, port, token, http: reqwest::Client::new() }
    }
    fn url(&self, p: &str) -> String {
        format!("http://127.0.0.1:{}{p}", self.port)
    }
    async fn post<T: serde::Serialize>(&self, p: &str, body: &T) -> reqwest::Response {
        self.http.post(self.url(p)).bearer_auth(&self.token).json(body).send().await.unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ping_is_public_everything_else_needs_the_token() {
    let s = Srv::start().await;
    let ping: serde_json::Value = s.http.get(s.url("/api/v1/ping")).send().await.unwrap().json().await.unwrap();
    assert_eq!(ping["service"], "alice");
    assert!(ping["instance"].as_str().unwrap().len() == 26);
    assert_eq!(s.http.get(s.url("/api/v1/health")).send().await.unwrap().status(), 401);
    assert_eq!(s.http.get(s.url("/api/v1/health")).bearer_auth("wrong").send().await.unwrap().status(), 401);
    assert_eq!(s.http.get(s.url("/api/v1/health")).bearer_auth(&s.token).send().await.unwrap().status(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn host_and_origin_are_checked() {
    let s = Srv::start().await;
    let r = s.http.get(s.url("/api/v1/health")).bearer_auth(&s.token).header("Host", "evil.example:80").send().await.unwrap();
    assert_eq!(r.status(), 403);
    let r = s.http.get(s.url("/api/v1/ping")).header("Host", "evil.example").send().await.unwrap();
    assert_eq!(r.status(), 403);
    let r = s.http.get(s.url("/api/v1/health")).bearer_auth(&s.token).header("Origin", "https://evil.example").send().await.unwrap();
    assert_eq!(r.status(), 403);
    let r = s.http.get(s.url("/api/v1/health")).bearer_auth(&s.token).header("Origin", format!("http://127.0.0.1:{}", s.port)).send().await.unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn remember_recall_and_errors_over_http() {
    let s = Srv::start().await;
    let r: RememberResponse = s.post("/api/v1/remember", &RememberRequest { content: "HTTP round trip works".into(), ..Default::default() }).await.json().await.unwrap();
    assert_eq!(r.status, RememberStatus::New);
    let recall: RecallResponse = s.post("/api/v1/recall", &RecallRequest { query: "round trip".into(), format: OutputFormat::Json, ..Default::default() }).await.json().await.unwrap();
    assert_eq!(recall.results[0].id, r.id.unwrap());
    let bad = s.post("/api/v1/remember", &RememberRequest { content: " ".into(), ..Default::default() }).await;
    assert_eq!(bad.status(), 400);
    let body: serde_json::Value = bad.json().await.unwrap();
    assert_eq!(body["kind"], "invalid");
    assert!(body["error"].as_str().unwrap().contains("empty"));
    let missing = s.post("/api/v1/update", &serde_json::json!({"id": "NOPE", "action": "archive"})).await;
    assert_eq!(missing.status(), 404);
}
```

- [ ] **Step 5: Implement `http.rs`**

```rust
//! JSON API over loopback HTTP. The guard (token + Host + Origin) is the code verified in the spike.

use crate::service::Service;
use alice_core::api::*;
use alice_core::error::AliceError;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Guard {
    token: Arc<str>,
    allowed_hosts: Arc<[String; 2]>,
    allowed_origin: Arc<str>,
}

impl Guard {
    pub fn new(token: impl Into<Arc<str>>, port: u16) -> Self {
        Self {
            token: token.into(),
            allowed_hosts: Arc::new([format!("127.0.0.1:{port}"), format!("localhost:{port}")]),
            allowed_origin: format!("http://127.0.0.1:{port}").into(),
        }
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn host_of(req: &Request) -> Option<&str> {
    req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).or_else(|| req.uri().authority().map(|a| a.as_str()))
}

fn bearer(h: &HeaderMap) -> Option<&str> {
    let v = h.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, tok) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(tok.trim())
}

fn host_and_origin_ok(g: &Guard, req: &Request) -> Result<(), Response> {
    match host_of(req) {
        Some(h) if g.allowed_hosts.iter().any(|a| a == h) => {}
        _ => return Err((StatusCode::FORBIDDEN, "forbidden: host").into_response()),
    }
    if let Some(o) = req.headers().get(header::ORIGIN) {
        if o.as_bytes() != g.allowed_origin.as_bytes() {
            return Err((StatusCode::FORBIDDEN, "forbidden: origin").into_response());
        }
    }
    Ok(())
}

pub async fn guard(State(g): State<Guard>, req: Request, next: Next) -> Response {
    if let Err(r) = host_and_origin_ok(&g, &req) {
        return r;
    }
    match bearer(req.headers()) {
        Some(t) if ct_eq(t.as_bytes(), g.token.as_bytes()) => next.run(req).await,
        _ => (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], "unauthorized").into_response(),
    }
}

/// For the public ping: Host and Origin only.
pub async fn host_guard(State(g): State<Guard>, req: Request, next: Next) -> Response {
    match host_and_origin_ok(&g, &req) {
        Ok(()) => next.run(req).await,
        Err(r) => r,
    }
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
    kind: &'static str,
}

pub struct ApiError(pub AliceError);

impl From<AliceError> for ApiError {
    fn from(e: AliceError) -> Self {
        Self(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, kind) = match &self.0 {
            AliceError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid"),
            AliceError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            AliceError::ReadOnly(_) => (StatusCode::SERVICE_UNAVAILABLE, "read_only"),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        (status, Json(ErrorBody { error: self.0.to_string(), kind })).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

#[derive(Clone)]
struct AppState {
    svc: Arc<Service>,
    shutdown: CancellationToken,
}

#[derive(Debug, Serialize, serde::Deserialize)]
pub struct PingResponse {
    pub service: String,
    pub instance: String,
    pub version: String,
}

async fn ping(State(st): State<AppState>) -> Json<PingResponse> {
    Json(PingResponse { service: "alice".into(), instance: st.svc.instance().to_string(), version: env!("CARGO_PKG_VERSION").into() })
}
async fn health(State(st): State<AppState>) -> ApiResult<Health> {
    Ok(Json(st.svc.health().await?))
}
async fn projects(State(st): State<AppState>) -> ApiResult<Vec<alice_core::store::projects::ResolvedProject>> {
    Ok(Json(st.svc.projects().await?))
}
async fn remember(State(st): State<AppState>, Json(req): Json<RememberRequest>) -> Result<Response, ApiError> {
    match st.svc.remember(req.clone()).await {
        Ok(r) => Ok(Json(r).into_response()),
        Err(AliceError::ReadOnly(_)) => {
            crate::spool::append(&st.svc.home().spool_dir(), &crate::spool::SpoolEntry::Remember(req))
                .map_err(|e| ApiError(AliceError::Io(e)))?;
            Ok((StatusCode::ACCEPTED, Json(RememberResponse { id: None, status: RememberStatus::Spooled, project: String::new() })).into_response())
        }
        Err(e) => Err(e.into()),
    }
}
async fn recall(State(st): State<AppState>, Json(req): Json<RecallRequest>) -> ApiResult<RecallResponse> {
    Ok(Json(st.svc.recall(req).await?))
}
async fn context(State(st): State<AppState>, Json(req): Json<ContextRequest>) -> ApiResult<ContextResponse> {
    Ok(Json(st.svc.context(req).await?))
}
async fn get_memories(State(st): State<AppState>, Json(req): Json<GetRequest>) -> ApiResult<GetResponse> {
    Ok(Json(st.svc.get(req).await?))
}
async fn update(State(st): State<AppState>, Json(req): Json<UpdateRequest>) -> ApiResult<MemoryFull> {
    Ok(Json(st.svc.update(req).await?))
}
async fn facts(State(st): State<AppState>, Json(req): Json<FactsRequest>) -> ApiResult<FactsResponse> {
    Ok(Json(st.svc.facts(req).await?))
}
async fn assert_fact(State(st): State<AppState>, Json(req): Json<AssertFactRequest>) -> ApiResult<alice_core::store::kg::FactOutcome> {
    Ok(Json(st.svc.assert_fact(req).await?))
}
async fn ingest(State(st): State<AppState>, Json(req): Json<IngestRequest>) -> ApiResult<alice_core::store::sources::IngestOutcome> {
    Ok(Json(st.svc.ingest(req).await?))
}
async fn shutdown(State(st): State<AppState>) -> StatusCode {
    st.shutdown.cancel();
    StatusCode::ACCEPTED
}

/// Build the router. Task 12 adds the `/mcp` service inside the token-guarded part.
pub fn router(svc: Arc<Service>, token: String, port: u16, shutdown: CancellationToken) -> Router {
    let g = Guard::new(token, port);
    let state = AppState { svc, shutdown };
    let public = Router::new()
        .route("/api/v1/ping", get(ping))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(g.clone(), host_guard));
    let private = Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/projects", get(projects))
        .route("/api/v1/remember", post(remember))
        .route("/api/v1/recall", post(recall))
        .route("/api/v1/context", post(context))
        .route("/api/v1/get", post(get_memories))
        .route("/api/v1/update", post(update))
        .route("/api/v1/facts/query", post(facts))
        .route("/api/v1/facts/assert", post(assert_fact))
        .route("/api/v1/ingest", post(ingest))
        .route("/api/v1/admin/shutdown", post(shutdown))
        .with_state(state)
        .layer(DefaultBodyLimit::max(40 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(g, guard));
    public.merge(private)
}
```

> `RememberRequest` needs `Clone` (already derived in Task 8) so the handler can spool it on a read-only error.

- [ ] **Step 6: Implement `daemon.rs`**

```rust
//! `alice daemon run`: single instance, token, discovery file, HTTP server, spool drain, clean shutdown.

use crate::discovery::{DaemonInfo, load_or_create_token, remove_info_if, write_info};
use crate::paths::AliceHome;
use crate::service::{Service, ServiceOptions};
use crate::spool;
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::ids::now_rfc3339;
use std::fs::{OpenOptions, TryLockError};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default)]
pub struct DaemonOptions {
    /// 0 = pick a free port.
    pub port: u16,
    /// Tests and CI: deterministic embeddings, no model download (`ALICE_EMBEDDER=fake`).
    pub fake_embedder: bool,
}

pub fn run(home: AliceHome, opts: DaemonOptions) -> anyhow::Result<()> {
    crate::install_tls();
    home.ensure_dirs()?;
    // Single instance. The lock lives in its own file because Windows locks are mandatory.
    let mut lock = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(home.lock_file())?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            eprintln!("alice daemon is already running");
            return Ok(());
        }
        Err(TryLockError::Error(e)) => return Err(e.into()),
    }
    lock.set_len(0)?;
    writeln!(lock, "{}", std::process::id())?;
    // Always printed (not subject to ALICE_LOG): process tests count this line to prove a single instance.
    eprintln!("alice daemon started pid={} home={}", std::process::id(), home.root().display());

    let token = load_or_create_token(&home)?;
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let models = home.models_dir();
    if !opts.fake_embedder && !alice_embed::is_model_present(&models) {
        tracing::info!("downloading the embedding model (first run, ~136 MB)");
        runtime.block_on(alice_embed::fetch_model(&models, &|_, _, _| {}))?;
    }
    let fake = opts.fake_embedder;
    let make = move || -> alice_core::Result<Arc<dyn Embedder>> {
        if fake {
            Ok(Arc::new(FakeEmbedder::new(384)))
        } else {
            Ok(Arc::new(alice_embed::FastEmbedder::new(&models)?))
        }
    };
    let svc = Service::start(home.clone(), &make, ServiceOptions::default())?;

    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", opts.port)).await?;
        let port = listener.local_addr()?.port();
        write_info(&home, &DaemonInfo {
            pid: std::process::id(),
            port,
            instance: svc.instance().to_string(),
            version: env!("CARGO_PKG_VERSION").into(),
            started_at: now_rfc3339(),
        })?;
        let ct = CancellationToken::new();
        let app = crate::http::router(svc.clone(), token, port, ct.clone());

        // Spool drain: now and every 5 s.
        {
            let svc = svc.clone();
            let ct = ct.clone();
            let dir = home.spool_dir();
            tokio::spawn(async move {
                loop {
                    if svc.health().await.is_ok_and(|h| h.read_only.is_none()) {
                        let r = spool::drain(&dir, &svc).await;
                        if r.processed + r.rejected > 0 {
                            tracing::info!(?r, "spool drained");
                        }
                    }
                    tokio::select! {
                        _ = ct.cancelled() => break,
                        _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                    }
                }
            });
        }
        // Signals.
        {
            let ct = ct.clone();
            tokio::spawn(async move {
                #[cfg(unix)]
                {
                    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("sigterm handler");
                    tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
                }
                #[cfg(not(unix))]
                let _ = tokio::signal::ctrl_c().await;
                ct.cancel();
            });
        }
        tracing::info!(port, "listening on 127.0.0.1");
        let server = axum::serve(listener, app).with_graceful_shutdown({
            let ct = ct.clone();
            async move { ct.cancelled().await }
        });
        // Open SSE/MCP streams would hold graceful shutdown open: cap it at 2 s.
        tokio::select! {
            r = server => r?,
            _ = async { ct.cancelled().await; tokio::time::sleep(Duration::from_secs(2)).await } => {
                tracing::warn!("graceful shutdown timed out");
            }
        }
        anyhow::Ok(())
    })?;

    svc.shutdown();
    std::thread::sleep(Duration::from_millis(200)); // let the writer flush its last batch
    remove_info_if(&home, svc.instance());
    let _ = lock.unlock();
    tracing::info!("daemon stopped");
    Ok(())
}
```

- [ ] **Step 7: Run tests**

Run: `nice -n 15 cargo test -j 6 -p alice-daemon`
Expected: PASS (discovery, spool, http_api, plus Task 10 tests).

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml crates/alice-daemon
git commit -m "feat(daemon): guarded loopback HTTP API, spool, discovery file and daemon lifecycle

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 12: Daemon client and MCP server (HTTP + stdio shim)

**Files:**
- Create: `crates/alice-daemon/src/{client.rs,mcp.rs}`
- Modify: `crates/alice-daemon/src/{lib.rs,http.rs}` (nest `/mcp`), `crates/alice-daemon/Cargo.toml`, `Cargo.toml` (workspace deps `rmcp`, `url`, `libc`)
- Test: `crates/alice-daemon/tests/mcp.rs`, unit tests in `client.rs`

**Interfaces:**
- Consumes: `http::router`, `discovery::*`, `spool::*`, `service::Service`, `alice_core::api::*`
- Produces:
  - `client::{Launcher { None, Exe(PathBuf) }, DaemonClient::{connect(home: &AliceHome, launcher: &Launcher, wait: Duration) -> Result<DaemonClient> (async), try_existing(home: &AliceHome) -> Option<DaemonClient> (async), health, remember, recall, context, get, update, facts, assert_fact, ingest, projects, shutdown (all async, typed like Service)}, remember_or_spool(home, launcher, req: RememberRequest) -> Result<RememberResponse> (async), ingest_or_spool(home, launcher, req: IngestRequest) -> Result<alice_core::store::sources::IngestOutcome> (async; spooled ingest returns inserted 0), spawn_detached(exe: &Path, args: &[&str], log: &Path) -> std::io::Result<u32>}`
  - `mcp::{Backend { Local(Arc<Service>), Remote(Arc<RemoteBackend>) }, RemoteBackend::new(home: AliceHome, launcher: Launcher, cwd: Option<String>) -> RemoteBackend, AliceMcp::new(backend: Backend) -> AliceMcp, INSTRUCTIONS: &str, http_service(svc: Arc<Service>, port: u16, ct: CancellationToken) -> StreamableHttpService<AliceMcp, LocalSessionManager>, run_stdio(home: AliceHome, launcher: Launcher) -> anyhow::Result<()> (async)}`
  - MCP surface: tools `recall`, `remember`, `context`, `get_memory`, `update_memory`, `query_facts`, `assert_fact`; prompts `recall` (arg `query`) and `remember-session`; resources `memory://context` (listed) and `memory://m/{id}` (readable).

- [ ] **Step 1: Dependencies**

Workspace additions:
```toml
rmcp = { version = "3.5.1", features = ["server", "macros", "client", "transport-io", "transport-child-process", "transport-streamable-http-server", "transport-streamable-http-client-reqwest"] }
url = "2.5.8"
libc = "0.2.190"
```
`alice-daemon` adds `rmcp`, `url`, and `[target.'cfg(unix)'.dependencies] libc.workspace = true`.

- [ ] **Step 2: Implement `client.rs`**

```rust
//! Find, verify, start and call the daemon. The token is sent only after `/api/v1/ping`
//! answers with the instance id written in daemon.json, so a stale file pointing at a
//! port now owned by another program never leaks the token.

use crate::discovery::{read_info, read_token};
use crate::http::PingResponse;
use crate::paths::AliceHome;
use crate::spool::{self, SpoolEntry};
use alice_core::api::*;
use alice_core::error::{AliceError, Result};
use alice_core::store::kg::FactOutcome;
use alice_core::store::projects::ResolvedProject;
use alice_core::store::sources::IngestOutcome;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum Launcher {
    /// Never start a daemon (tests, explicit opt-out).
    None,
    /// Start `<exe> daemon run` detached.
    Exe(PathBuf),
}

#[derive(Clone)]
pub struct DaemonClient {
    http: reqwest::Client,
    base: String,
    token: String,
}

fn transport(e: reqwest::Error) -> AliceError {
    AliceError::Other(format!("memory daemon unreachable: {e}. It restarts automatically; retry in a few seconds"))
}

/// Detach so the daemon survives the caller (agent hooks, shims, terminals).
pub fn spawn_detached(exe: &Path, args: &[&str], log: &Path) -> std::io::Result<u32> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    let err = out.try_clone()?;
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::from(out)).stderr(Stdio::from(err));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.current_dir("/");
        // SAFETY: setsid is async-signal-safe and called in the child before exec.
        unsafe {
            cmd.pre_exec(|| if libc::setsid() == -1 { Err(std::io::Error::last_os_error()) } else { Ok(()) });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn()?;
    let pid = child.id();
    // Reap it if it exits while we are alive (no zombie); does not keep it attached.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(pid)
}

impl DaemonClient {
    pub async fn try_existing(home: &AliceHome) -> Option<DaemonClient> {
        let info = read_info(home)?;
        let token = read_token(home)?;
        let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().ok()?;
        let base = format!("http://127.0.0.1:{}", info.port);
        let ping: PingResponse = http
            .get(format!("{base}/api/v1/ping"))
            .timeout(Duration::from_millis(300))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        (ping.service == "alice" && ping.instance == info.instance).then_some(DaemonClient { http, base, token })
    }

    pub async fn connect(home: &AliceHome, launcher: &Launcher, wait: Duration) -> Result<DaemonClient> {
        if let Some(c) = Self::try_existing(home).await {
            return Ok(c);
        }
        let Launcher::Exe(exe) = launcher else {
            return Err(AliceError::Other("memory daemon is not running; start it with \"alice daemon start\"".into()));
        };
        spawn_detached(exe, &["daemon", "run"], &home.logs_dir().join("daemon.log"))?;
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Some(c) = Self::try_existing(home).await {
                return Ok(c);
            }
        }
        Err(AliceError::Other("memory daemon is starting; retry in a few seconds".into()))
    }

    async fn call<Req: Serialize + ?Sized, Resp: DeserializeOwned>(&self, path: &str, req: &Req, timeout: Duration) -> Result<Resp> {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .timeout(timeout)
            .json(req)
            .send()
            .await
            .map_err(transport)?;
        Self::decode(resp).await
    }

    async fn get_json<Resp: DeserializeOwned>(&self, path: &str) -> Result<Resp> {
        let resp = self.http.get(format!("{}{path}", self.base)).bearer_auth(&self.token).send().await.map_err(transport)?;
        Self::decode(resp).await
    }

    async fn decode<Resp: DeserializeOwned>(resp: reqwest::Response) -> Result<Resp> {
        if resp.status().is_success() {
            return resp.json().await.map_err(|e| AliceError::Other(format!("bad daemon response: {e}")));
        }
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let msg = body["error"].as_str().unwrap_or("request failed").to_string();
        Err(match body["kind"].as_str() {
            Some("invalid") => AliceError::Invalid(msg),
            Some("not_found") => AliceError::NotFound(msg.trim_start_matches("not found: ").to_string()),
            Some("read_only") => AliceError::ReadOnly(msg),
            _ => AliceError::Other(msg),
        })
    }

    pub async fn health(&self) -> Result<Health> {
        self.get_json("/api/v1/health").await
    }
    pub async fn projects(&self) -> Result<Vec<ResolvedProject>> {
        self.get_json("/api/v1/projects").await
    }
    pub async fn remember(&self, r: &RememberRequest) -> Result<RememberResponse> {
        self.call("/api/v1/remember", r, Duration::from_secs(5)).await
    }
    pub async fn recall(&self, r: &RecallRequest) -> Result<RecallResponse> {
        self.call("/api/v1/recall", r, Duration::from_secs(5)).await
    }
    pub async fn context(&self, r: &ContextRequest) -> Result<ContextResponse> {
        self.call("/api/v1/context", r, Duration::from_secs(3)).await
    }
    pub async fn get(&self, r: &GetRequest) -> Result<GetResponse> {
        self.call("/api/v1/get", r, Duration::from_secs(5)).await
    }
    pub async fn update(&self, r: &UpdateRequest) -> Result<MemoryFull> {
        self.call("/api/v1/update", r, Duration::from_secs(5)).await
    }
    pub async fn facts(&self, r: &FactsRequest) -> Result<FactsResponse> {
        self.call("/api/v1/facts/query", r, Duration::from_secs(5)).await
    }
    pub async fn assert_fact(&self, r: &AssertFactRequest) -> Result<FactOutcome> {
        self.call("/api/v1/facts/assert", r, Duration::from_secs(5)).await
    }
    pub async fn ingest(&self, r: &IngestRequest) -> Result<IngestOutcome> {
        self.call("/api/v1/ingest", r, Duration::from_secs(30)).await
    }
    pub async fn shutdown(&self) -> Result<()> {
        let resp = self.http.post(format!("{}/api/v1/admin/shutdown", self.base)).bearer_auth(&self.token).send().await.map_err(transport)?;
        if resp.status().is_success() { Ok(()) } else { Err(AliceError::Other(format!("shutdown refused: {}", resp.status()))) }
    }
}

fn is_unavailable(e: &AliceError) -> bool {
    matches!(e, AliceError::Other(m) if m.starts_with("memory daemon")) || matches!(e, AliceError::ReadOnly(_))
}

/// Writes never fail because the daemon is down: they are spooled and applied when it is back.
pub async fn remember_or_spool(home: &AliceHome, launcher: &Launcher, req: RememberRequest) -> Result<RememberResponse> {
    let spooled = |req: RememberRequest| -> Result<RememberResponse> {
        spool::append(&home.spool_dir(), &SpoolEntry::Remember(req))?;
        Ok(RememberResponse { id: None, status: RememberStatus::Spooled, project: String::new() })
    };
    match DaemonClient::connect(home, launcher, Duration::from_millis(1_500)).await {
        Ok(c) => match c.remember(&req).await {
            Err(e) if is_unavailable(&e) => spooled(req),
            other => other,
        },
        Err(_) => spooled(req),
    }
}

pub async fn ingest_or_spool(home: &AliceHome, launcher: &Launcher, req: IngestRequest) -> Result<IngestOutcome> {
    alice_core::store::sources::validate_chunks(&req.chunks)?;
    let spooled = |req: IngestRequest| -> Result<IngestOutcome> {
        spool::append(&home.spool_dir(), &SpoolEntry::Ingest(req))?;
        Ok(IngestOutcome { source_id: String::new(), inserted: 0, skipped: 0 })
    };
    match DaemonClient::connect(home, launcher, Duration::from_millis(1_500)).await {
        Ok(c) => match c.ingest(&req).await {
            Err(e) if is_unavailable(&e) => spooled(req),
            other => other,
        },
        Err(_) => spooled(req),
    }
}
```

- [ ] **Step 3: Implement `mcp.rs`**

```rust
//! The MCP server. The same `AliceMcp` runs inside the daemon (Streamable HTTP at /mcp,
//! backend = the in-process Service) and in `alice mcp` (stdio, backend = HTTP to the daemon).

#![allow(deprecated)] // roots are deprecated by SEP-2577 but current clients still provide them

use crate::client::{DaemonClient, Launcher, remember_or_spool};
use crate::paths::AliceHome;
use crate::service::Service;
use alice_core::api::*;
use alice_core::error::{AliceError, Result};
use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ListResourcesResult, PaginatedRequestParams, PromptMessage,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource, ResourceContents, Role,
    ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt, prompt, prompt_handler, prompt_router, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const INSTRUCTIONS: &str = "Alice is your long-term memory, shared with the user's other agents and sessions.\n\
- When you start work on a project, call `context` unless a memory briefing is already in the conversation.\n\
- Before answering about past work, decisions, bugs or preferences, call `recall`.\n\
- Call `remember` for decisions (with the reason), bug fixes (symptom, cause, fix), facts, how-tos, warnings and user preferences. One idea per memory; set `kind`.\n\
- When a new memory replaces an old one, pass the old id in `supersedes`.\n\
- Before ending a long task, `remember` a `handoff`: what is done, what is next, open questions.\n\
- Use `assert_fact` and `query_facts` for stable relationships (X uses Y, service runs on host).\n\
- Never store secrets.";

pub struct RemoteBackend {
    home: AliceHome,
    launcher: Launcher,
    cwd: Option<String>,
    client: tokio::sync::Mutex<Option<DaemonClient>>,
}

impl RemoteBackend {
    pub fn new(home: AliceHome, launcher: Launcher, cwd: Option<String>) -> Self {
        Self { home, launcher, cwd, client: tokio::sync::Mutex::new(None) }
    }

    /// Connect lazily (keeps `alice mcp` start-up fast); reconnect when the daemon restarted.
    async fn client(&self) -> Result<DaemonClient> {
        let mut slot = self.client.lock().await;
        if let Some(c) = slot.as_ref() {
            if c.health().await.is_ok() {
                return Ok(c.clone());
            }
        }
        let c = DaemonClient::connect(&self.home, &self.launcher, Duration::from_secs(5)).await?;
        *slot = Some(c.clone());
        Ok(c)
    }
}

#[derive(Clone)]
pub enum Backend {
    Local(Arc<Service>),
    Remote(Arc<RemoteBackend>),
}

impl Backend {
    fn default_cwd(&self) -> Option<String> {
        match self {
            Backend::Local(_) => None,
            Backend::Remote(r) => r.cwd.clone(),
        }
    }
    async fn recall(&self, r: RecallRequest) -> Result<RecallResponse> {
        match self { Backend::Local(s) => s.recall(r).await, Backend::Remote(b) => b.client().await?.recall(&r).await }
    }
    async fn remember(&self, r: RememberRequest) -> Result<RememberResponse> {
        match self { Backend::Local(s) => s.remember(r).await, Backend::Remote(b) => remember_or_spool(&b.home, &b.launcher, r).await }
    }
    async fn context(&self, r: ContextRequest) -> Result<ContextResponse> {
        match self { Backend::Local(s) => s.context(r).await, Backend::Remote(b) => b.client().await?.context(&r).await }
    }
    async fn get(&self, r: GetRequest) -> Result<GetResponse> {
        match self { Backend::Local(s) => s.get(r).await, Backend::Remote(b) => b.client().await?.get(&r).await }
    }
    async fn update(&self, r: UpdateRequest) -> Result<MemoryFull> {
        match self { Backend::Local(s) => s.update(r).await, Backend::Remote(b) => b.client().await?.update(&r).await }
    }
    async fn facts(&self, r: FactsRequest) -> Result<FactsResponse> {
        match self { Backend::Local(s) => s.facts(r).await, Backend::Remote(b) => b.client().await?.facts(&r).await }
    }
    async fn assert_fact(&self, r: AssertFactRequest) -> Result<alice_core::store::kg::FactOutcome> {
        match self { Backend::Local(s) => s.assert_fact(r).await, Backend::Remote(b) => b.client().await?.assert_fact(&r).await }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecallPromptArgs {
    /// What to look for.
    pub query: String,
}

#[derive(Clone)]
pub struct AliceMcp {
    backend: Backend,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
}

fn text(s: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(s.into())])
}

fn tool_err(e: AliceError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(e.to_string())])
}

fn json<T: serde::Serialize>(v: &T) -> CallToolResult {
    text(serde_json::to_string_pretty(v).unwrap_or_default())
}

impl AliceMcp {
    pub fn new(backend: Backend) -> Self {
        Self { backend, tool_router: Self::tool_router(), prompt_router: Self::prompt_router() }
    }

    /// Working directory for project detection: the shim's cwd, else the client's first root, else none.
    async fn cwd(&self, ctx: &RequestContext<RoleServer>) -> Option<String> {
        if let Some(c) = self.backend.default_cwd() {
            return Some(c);
        }
        let roots = tokio::time::timeout(Duration::from_millis(300), ctx.peer.list_roots()).await.ok()?.ok()?;
        let first = roots.roots.first()?;
        url::Url::parse(&first.uri).ok()?.to_file_path().ok().map(|p| p.to_string_lossy().into_owned())
    }

    fn agent(ctx: &RequestContext<RoleServer>) -> Option<String> {
        ctx.peer.peer_info().map(|i| i.client_info.name.clone())
    }
}

#[tool_router]
impl AliceMcp {
    #[tool(description = "Search long-term memory: decisions, bug fixes, facts, how-tos, preferences and past work. Use before answering about anything done before.", annotations(read_only_hint = true))]
    async fn recall(&self, Parameters(mut req): Parameters<RecallRequest>, ctx: RequestContext<RoleServer>) -> std::result::Result<CallToolResult, McpError> {
        req.cwd = self.cwd(&ctx).await;
        let format = req.format;
        Ok(match self.backend.recall(req).await {
            Ok(r) if format == OutputFormat::Json => json(&r),
            Ok(r) => text(r.text.unwrap_or_default()),
            Err(e) => tool_err(e),
        })
    }

    #[tool(description = "Save one memory worth keeping: a decision (with why), bug fix (symptom, cause, fix), fact, how-to, preference, warning or end-of-task handoff. Set kind; pass supersedes to replace older memories.")]
    async fn remember(&self, Parameters(mut req): Parameters<RememberRequest>, ctx: RequestContext<RoleServer>) -> std::result::Result<CallToolResult, McpError> {
        req.cwd = self.cwd(&ctx).await;
        if req.author.agent.is_none() {
            req.author.agent = Self::agent(&ctx);
        }
        Ok(match self.backend.remember(req).await {
            Ok(r) => match (r.status, r.id) {
                (RememberStatus::Spooled, _) => text("Queued: the memory service is starting; this memory will be saved within seconds."),
                (RememberStatus::Duplicate, Some(id)) => text(format!("Already remembered as {id} in {}.", r.project)),
                (_, Some(id)) => text(format!("Saved {id} in {}.", r.project)),
                (_, None) => text("Saved."),
            },
            Err(e) => tool_err(e),
        })
    }

    #[tool(description = "Briefing for the current project: key memories, warnings, recent decisions, last handoff and the user's preferences. Call at the start of work.", annotations(read_only_hint = true))]
    async fn context(&self, Parameters(mut req): Parameters<ContextRequest>, ctx: RequestContext<RoleServer>) -> std::result::Result<CallToolResult, McpError> {
        req.cwd = self.cwd(&ctx).await;
        Ok(match self.backend.context(req).await {
            Ok(r) => text(r.text),
            Err(e) => tool_err(e),
        })
    }

    #[tool(description = "Fetch full memories by id, with links and optionally the raw session chunks they came from.", annotations(read_only_hint = true))]
    async fn get_memory(&self, Parameters(req): Parameters<GetRequest>) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.backend.get(req).await {
            Ok(r) => json(&r),
            Err(e) => tool_err(e),
        })
    }

    #[tool(description = "Change a memory: action edit (title/body/kind/topics/importance), supersede (superseded_by), archive, forget (soft delete) or restore.")]
    async fn update_memory(&self, Parameters(req): Parameters<UpdateRequest>) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.backend.update(req).await {
            Ok(r) => json(&r),
            Err(e) => tool_err(e),
        })
    }

    #[tool(description = "Facts about an entity from the knowledge graph: current ones, as of a date, or the full timeline.", annotations(read_only_hint = true))]
    async fn query_facts(&self, Parameters(req): Parameters<FactsRequest>) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.backend.facts(req).await {
            Ok(r) => text(r.text),
            Err(e) => tool_err(e),
        })
    }

    #[tool(description = "Record a fact (subject, predicate, object). action add, supersede (replace the current value) or end (it stopped being true).")]
    async fn assert_fact(&self, Parameters(req): Parameters<AssertFactRequest>) -> std::result::Result<CallToolResult, McpError> {
        Ok(match self.backend.assert_fact(req).await {
            Ok(r) => json(&r),
            Err(e) => tool_err(e),
        })
    }
}

#[prompt_router]
impl AliceMcp {
    #[prompt(name = "recall", description = "Search Alice memory for a topic")]
    async fn recall_prompt(&self, Parameters(a): Parameters<RecallPromptArgs>) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(Role::User, format!("Search my memory for: {}. Use the recall tool, then summarise what is relevant and cite memory ids.", a.query))]
    }

    #[prompt(name = "remember-session", description = "Save what this session learned")]
    async fn remember_session(&self) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            Role::User,
            "Review this session and save what is worth keeping with the remember tool: decisions (with reasons), bug fixes (symptom, cause, fix), facts, how-tos, warnings and my preferences. One idea per memory, with kind set. Then save a handoff memory: what is done, what is next, open questions.",
        )]
    }
}

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for AliceMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_prompts().enable_resources().build())
            .with_server_info(Implementation::new("alice", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_resources(&self, _r: Option<PaginatedRequestParams>, _ctx: RequestContext<RoleServer>) -> std::result::Result<ListResourcesResult, McpError> {
        let r = Resource::new("memory://context", "Project memory briefing").with_description("Alice context for the current project").with_mime_type("text/plain");
        Ok(ListResourcesResult::with_all_items(vec![r]))
    }

    async fn read_resource(&self, request: ReadResourceRequestParams, ctx: RequestContext<RoleServer>) -> std::result::Result<ReadResourceResponse, McpError> {
        let uri = request.uri.clone();
        let body = if uri == "memory://context" {
            let cwd = self.cwd(&ctx).await;
            self.backend.context(ContextRequest { cwd, ..Default::default() }).await.map(|r| r.text)
        } else if let Some(id) = uri.strip_prefix("memory://m/") {
            self.backend.get(GetRequest { ids: vec![id.to_string()], include_provenance: true }).await.map(|r| serde_json::to_string_pretty(&r).unwrap_or_default())
        } else {
            return Err(McpError::resource_not_found(format!("unknown resource {uri}"), None));
        };
        let body = body.map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(ReadResourceResult::new(vec![ResourceContents::text(body, uri)]).into())
    }
}

/// Streamable HTTP service mounted at `/mcp` inside the token-guarded router.
pub fn http_service(svc: Arc<Service>, port: u16, ct: CancellationToken) -> StreamableHttpService<AliceMcp, LocalSessionManager> {
    let server = AliceMcp::new(Backend::Local(svc));
    StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_allowed_hosts([format!("127.0.0.1:{port}"), format!("localhost:{port}")])
            .with_allowed_origins([format!("http://127.0.0.1:{port}")])
            .with_cancellation_token(ct.child_token()),
    )
}

/// `alice mcp`: stdio server for agents that launch MCP servers as child processes.
/// Logs must go to stderr; stdout carries the protocol.
pub async fn run_stdio(home: AliceHome, launcher: Launcher) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned());
    let server = AliceMcp::new(Backend::Remote(Arc::new(RemoteBackend::new(home, launcher, cwd))));
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}
```

> If the `#[prompt]` macro rejects a prompt without parameters, give `remember_session` an empty args struct (`#[derive(Deserialize, JsonSchema)] pub struct NoArgs {}`) taken as `Parameters(_): Parameters<NoArgs>`.

Mount it in `http::router` inside the private (guarded) router, before `.with_state(state)`:
```rust
        .nest_service("/mcp", crate::mcp::http_service(state.svc.clone(), port, state.shutdown.clone()))
```
(Order matters: `.layer(guard)` must come after `nest_service` so `/mcp` is covered; the spike verified `/mcp` without a token returns 401.)

- [ ] **Step 4: MCP tests (`crates/alice-daemon/tests/mcp.rs`)**

```rust
#![allow(deprecated)]

use alice_core::embed::{Embedder, FakeEmbedder};
use alice_daemon::client::Launcher;
use alice_daemon::discovery::{DaemonInfo, write_info};
use alice_daemon::mcp::{AliceMcp, Backend, RemoteBackend};
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use rmcp::model::{CallToolRequestParams, ClientCapabilities, ClientConfig, GetPromptRequestParams, Implementation, ReadResourceRequestParams};
use rmcp::transport::streamable_http_client::{StreamableHttpClientTransport, StreamableHttpClientTransportConfig};
use rmcp::{ClientHandler, ServiceExt};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct TestClient;
impl ClientHandler for TestClient {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(ClientCapabilities::default(), Implementation::new("test-agent", "1.0"))
    }
}

fn args(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().unwrap().clone()
}

fn first_text(r: &rmcp::model::CallToolResult) -> String {
    r.content.first().and_then(|c| c.as_text()).map(|t| t.text.clone()).unwrap_or_default()
}

async fn daemon() -> (tempfile::TempDir, AliceHome, u16, String) {
    alice_daemon::install_tls();
    let d = tempfile::tempdir().unwrap();
    let home = AliceHome::new(d.path().to_path_buf());
    let svc = Service::start(home.clone(), &|| Ok(Arc::new(FakeEmbedder::new(384)) as Arc<dyn Embedder>), ServiceOptions::default()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = alice_daemon::discovery::load_or_create_token(&home).unwrap();
    write_info(&home, &DaemonInfo { pid: std::process::id(), port, instance: svc.instance().into(), version: "t".into(), started_at: "t".into() }).unwrap();
    let app = alice_daemon::http::router(svc, token.clone(), port, CancellationToken::new());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (d, home, port, token)
}

#[tokio::test(flavor = "multi_thread")]
async fn http_mcp_exposes_exactly_seven_tools_and_works() {
    let (_d, _home, port, token) = daemon().await;
    let cfg = StreamableHttpClientTransportConfig::with_uri(format!("http://127.0.0.1:{port}/mcp")).auth_header(token);
    let client = TestClient.serve(StreamableHttpClientTransport::from_config(cfg)).await.unwrap();
    let info = client.peer_info().unwrap();
    assert!(info.instructions.as_deref().unwrap().contains("long-term memory"));
    let mut names: Vec<String> = client.list_all_tools().await.unwrap().into_iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(names, ["assert_fact", "context", "get_memory", "query_facts", "recall", "remember", "update_memory"]);
    let saved = client.call_tool(CallToolRequestParams::new("remember").with_arguments(args(serde_json::json!({"content": "MCP over HTTP stores memories", "kind": "fact"})))).await.unwrap();
    assert!(first_text(&saved).starts_with("Saved "), "{}", first_text(&saved));
    let found = client.call_tool(CallToolRequestParams::new("recall").with_arguments(args(serde_json::json!({"query": "MCP over HTTP"})))).await.unwrap();
    assert!(first_text(&found).contains("MCP over HTTP stores memories"));
    let bad = client.call_tool(CallToolRequestParams::new("remember").with_arguments(args(serde_json::json!({"content": "  "})))).await.unwrap();
    assert_eq!(bad.is_error, Some(true));
    let p = client.get_prompt(GetPromptRequestParams::new("remember-session")).await.unwrap();
    assert!(!p.messages.is_empty());
    let ctx = client.read_resource(ReadResourceRequestParams::new("memory://context")).await.unwrap();
    assert!(!ctx.contents.is_empty());
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_without_token_is_rejected() {
    let (_d, _home, port, _token) = daemon().await;
    let r = reqwest::Client::new().post(format!("http://127.0.0.1:{port}/mcp")).body("{}").send().await.unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn stdio_style_remote_backend_uses_the_daemon_and_its_cwd() {
    let (d, home, _port, _token) = daemon().await;
    let repo = d.path().join("shimproj");
    std::fs::create_dir(&repo).unwrap();
    let server = AliceMcp::new(Backend::Remote(Arc::new(RemoteBackend::new(home, Launcher::None, Some(repo.to_string_lossy().into_owned())))));
    let (a, b) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move { server.serve(a).await.unwrap().waiting().await });
    let client = TestClient.serve(b).await.unwrap();
    let saved = client.call_tool(CallToolRequestParams::new("remember").with_arguments(args(serde_json::json!({"content": "remote backend writes"})))).await.unwrap();
    assert!(first_text(&saved).contains("in shimproj"), "{}", first_text(&saved));
    client.cancel().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_backend_spools_when_no_daemon() {
    alice_daemon::install_tls();
    let d = tempfile::tempdir().unwrap();
    let home = AliceHome::new(d.path().to_path_buf());
    home.ensure_dirs().unwrap();
    let server = AliceMcp::new(Backend::Remote(Arc::new(RemoteBackend::new(home.clone(), Launcher::None, None))));
    let (a, b) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move { server.serve(a).await.unwrap().waiting().await });
    let client = TestClient.serve(b).await.unwrap();
    let r = client.call_tool(CallToolRequestParams::new("remember").with_arguments(args(serde_json::json!({"content": "saved later"})))).await.unwrap();
    assert!(first_text(&r).starts_with("Queued"), "{}", first_text(&r));
    assert_eq!(alice_daemon::spool::pending_count(&home.spool_dir()), 1);
    client.cancel().await.unwrap();
}
```

- [ ] **Step 5: Run tests**

Run: `nice -n 15 cargo test -j 6 -p alice-daemon`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/alice-daemon
git commit -m "feat(mcp): seven-tool MCP server over Streamable HTTP and stdio, verified client with spool fallback

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 13: The `alice` binary (CLI, daemon entry, MCP shim) and process-level tests

**Files:**
- Create: `crates/alice/Cargo.toml`, `crates/alice/src/{main.rs,cmd.rs}`, `crates/alice/tests/cli.rs`
- Modify: `Cargo.toml` (members += `crates/alice`; workspace deps `clap`, `tracing-subscriber`)

**Interfaces:**
- Consumes: `alice_daemon::{daemon::{run, DaemonOptions}, client::*, mcp::run_stdio, paths::AliceHome, install_tls}`, `alice_core::api::*`, `alice_embed::fetch_model`
- Produces: the `alice` executable. Environment: `ALICE_HOME`, `ALICE_EMBEDDER=fake` (tests/CI only), `ALICE_AGENT` (author name for CLI writes, default `cli`), `ALICE_LOG` (tracing filter, default `info`).

- [ ] **Step 1: Manifest**

Workspace additions:
```toml
clap = { version = "4.6.7", features = ["derive", "env"] }
tracing-subscriber = { version = "0.3.23", features = ["env-filter"] }
```

`crates/alice/Cargo.toml`:
```toml
[package]
name = "alice-memory"
version.workspace = true
edition.workspace = true
license.workspace = true
repository.workspace = true
rust-version.workspace = true
description = "Alice: long-term memory for AI coding agents (MCP server, CLI and daemon in one binary)"
readme = "../../README.md"
keywords = ["mcp", "memory", "ai-agents", "claude-code", "codex"]
categories = ["command-line-utilities", "development-tools"]

[[bin]]
name = "alice"
path = "src/main.rs"

[dependencies]
alice-core = { path = "../alice-core" }
alice-daemon = { path = "../alice-daemon" }
alice-embed = { path = "../alice-embed" }
clap.workspace = true
tokio.workspace = true
serde_json.workspace = true
anyhow.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true

[target.'cfg(unix)'.dependencies]
libc.workspace = true

[dev-dependencies]
tempfile.workspace = true
rmcp.workspace = true
serde_json.workspace = true
```

- [ ] **Step 2: `main.rs`**

```rust
//! `alice`: long-term memory for AI coding agents.

mod cmd;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "alice", version, about = "Long-term memory for AI coding agents")]
struct Cli {
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Search memories.
    Recall(cmd::RecallArgs),
    /// Save a memory (text argument, or "-" to read stdin).
    Remember(cmd::RememberArgs),
    /// Briefing for the current project.
    Context(cmd::ContextArgs),
    /// Show memories by id.
    Get(cmd::GetArgs),
    /// Edit, supersede, archive, forget or restore a memory.
    Update(cmd::UpdateArgs),
    /// Facts about an entity.
    Facts(cmd::FactsArgs),
    /// Add, supersede or end a fact.
    Fact(cmd::FactArgs),
    /// List projects.
    Projects,
    /// Daemon status and counts.
    Status,
    /// Manage the background daemon.
    #[command(subcommand)]
    Daemon(cmd::DaemonCmd),
    /// Run the MCP server on stdio (configure agents to launch `alice mcp`).
    Mcp,
    /// Embedding model management.
    #[command(subcommand)]
    Model(cmd::ModelCmd),
    /// Measure latency and throughput on synthetic data.
    Bench(cmd::BenchArgs),
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    alice_daemon::install_tls();
    let is_server = matches!(cli.command, Command::Daemon(cmd::DaemonCmd::Run) | Command::Mcp);
    // Logs go to stderr: on stdio MCP, stdout is the protocol channel.
    let filter = tracing_subscriber::EnvFilter::try_from_env("ALICE_LOG").unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(if is_server { "info" } else { "warn" }));
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
    #[cfg(unix)]
    if !is_server {
        // `alice recall … | head` must not panic on a closed pipe.
        // SAFETY: restoring the default disposition before any output is written.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    match cmd::dispatch(cli.command, cli.json) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("alice: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
```

> `cmd.rs` is a child module of the crate root, so it can use the private `Command` enum via `use crate::Command;`.

- [ ] **Step 3: `cmd.rs`**

```rust
use crate::Command;
use alice_core::api::*;
use alice_core::model::Kind;
use alice_core::store::kg::{AssertFact, FactAction, FactsQuery};
use alice_core::store::memories::{MemoryUpdate, UpdateAction};
use alice_daemon::client::{DaemonClient, Launcher, remember_or_spool};
use alice_daemon::daemon::{DaemonOptions, run};
use alice_daemon::paths::AliceHome;
use anyhow::{Context as _, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use std::io::Read;
use std::time::Duration;

#[derive(Args)]
pub struct RecallArgs {
    /// Keywords or a question.
    #[arg(required = true)]
    pub query: Vec<String>,
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long = "kind")]
    pub kinds: Vec<String>,
    #[arg(long = "topic")]
    pub topics: Vec<String>,
    /// Also search raw session transcripts.
    #[arg(long)]
    pub raw: bool,
    #[arg(long)]
    pub limit: Option<usize>,
    #[arg(long)]
    pub max_tokens: Option<usize>,
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub before: Option<String>,
}

#[derive(Args)]
pub struct RememberArgs {
    /// Memory text, or "-" to read stdin.
    pub text: Vec<String>,
    #[arg(long)]
    pub kind: Option<String>,
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long = "topic")]
    pub topics: Vec<String>,
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long)]
    pub importance: Option<u8>,
    #[arg(long)]
    pub supersedes: Vec<String>,
    /// Author shown on the memory (default: $ALICE_AGENT or "cli").
    #[arg(long, env = "ALICE_AGENT", default_value = "cli")]
    pub agent: String,
}

#[derive(Args)]
pub struct ContextArgs {
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long)]
    pub max_tokens: Option<usize>,
}

#[derive(Args)]
pub struct GetArgs {
    #[arg(required = true)]
    pub ids: Vec<String>,
    #[arg(long)]
    pub provenance: bool,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum ActionArg {
    Edit,
    Supersede,
    Archive,
    Forget,
    Restore,
}

#[derive(Args)]
pub struct UpdateArgs {
    pub id: String,
    #[arg(long, value_enum)]
    pub action: ActionArg,
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long)]
    pub body: Option<String>,
    #[arg(long)]
    pub kind: Option<String>,
    #[arg(long)]
    pub importance: Option<u8>,
    /// For supersede: the id of the replacing memory.
    #[arg(long)]
    pub by: Option<String>,
}

#[derive(Args)]
pub struct FactsArgs {
    pub entity: String,
    #[arg(long)]
    pub predicate: Option<String>,
    #[arg(long)]
    pub as_of: Option<String>,
    #[arg(long)]
    pub timeline: bool,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum FactActionArg {
    Add,
    Supersede,
    End,
}

#[derive(Args)]
pub struct FactArgs {
    #[arg(value_enum)]
    pub action: FactActionArg,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    /// When it became (or, for end, stopped being) true.
    #[arg(long)]
    pub from: Option<String>,
}

#[derive(Subcommand)]
pub enum DaemonCmd {
    /// Run in the foreground.
    Run,
    /// Start in the background if not running.
    Start,
    /// Stop the running daemon.
    Stop,
    /// Show whether it runs.
    Status,
    /// Print the last lines of the daemon log.
    Logs,
}

#[derive(Subcommand)]
pub enum ModelCmd {
    /// Download the embedding model (~136 MB) for offline use.
    Fetch,
}

#[derive(Args)]
pub struct BenchArgs {
    #[arg(long, default_value_t = 2_000)]
    pub memories: usize,
    #[arg(long, default_value_t = 100_000)]
    pub chunks: usize,
    /// Use the real embedding model instead of the fake one.
    #[arg(long)]
    pub real: bool,
    /// Write results as JSON to this file.
    #[arg(long)]
    pub out: Option<std::path::PathBuf>,
}

fn rt() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread().enable_all().build()?)
}

fn launcher() -> Launcher {
    std::env::current_exe().map(Launcher::Exe).unwrap_or(Launcher::None)
}

fn cwd() -> Option<String> {
    std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned())
}

fn kind(s: &Option<String>) -> Result<Option<Kind>> {
    match s {
        None => Ok(None),
        Some(k) => Kind::parse(k).map(Some).with_context(|| {
            format!("unknown kind {k:?}; use one of: {}", Kind::ALL.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", "))
        }),
    }
}

fn print<T: serde::Serialize>(json: bool, value: &T, text: impl FnOnce() -> String) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        print!("{}", text());
    }
    Ok(())
}

async fn client(home: &AliceHome) -> Result<DaemonClient> {
    Ok(DaemonClient::connect(home, &launcher(), Duration::from_secs(10)).await?)
}

pub fn dispatch(command: Command, json: bool) -> Result<()> {
    let home = AliceHome::from_env();
    match command {
        Command::Daemon(DaemonCmd::Run) => run(home, DaemonOptions { port: 0, fake_embedder: std::env::var("ALICE_EMBEDDER").is_ok_and(|v| v == "fake") }),
        Command::Mcp => rt()?.block_on(alice_daemon::mcp::run_stdio(home, launcher())),
        Command::Model(ModelCmd::Fetch) => rt()?.block_on(async {
            alice_embed::fetch_model(&home.models_dir(), &|name, done, total| {
                eprint!("\r{name}: {:>5.1}%", done as f64 * 100.0 / total.max(1) as f64);
            })
            .await?;
            eprintln!("\nmodel ready in {}", alice_embed::model_dir(&home.models_dir()).display());
            Ok(())
        }),
        Command::Bench(args) => crate::cmd::bench(home, args, json),
        other => rt()?.block_on(client_command(home, other, json)),
    }
}

async fn client_command(home: AliceHome, command: Command, json: bool) -> Result<()> {
    match command {
        Command::Recall(a) => {
            let kinds = a.kinds.iter().map(|k| kind(&Some(k.clone())).map(Option::unwrap)).collect::<Result<Vec<_>>>()?;
            let req = RecallRequest {
                query: a.query.join(" "),
                project: a.project,
                topics: a.topics,
                kinds,
                since: a.since,
                before: a.before,
                include_raw: a.raw,
                limit: a.limit,
                max_tokens: a.max_tokens,
                format: if json { OutputFormat::Json } else { OutputFormat::Text },
                cwd: cwd(),
                ..Default::default()
            };
            let r = client(&home).await?.recall(&req).await?;
            print(json, &r, || r.text.clone().unwrap_or_default())
        }
        Command::Remember(a) => {
            let content = if a.text.len() == 1 && a.text[0] == "-" {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                s
            } else {
                a.text.join(" ")
            };
            if content.trim().is_empty() {
                bail!("nothing to remember: pass text or \"-\" to read stdin");
            }
            let req = RememberRequest {
                content,
                kind: kind(&a.kind)?,
                title: a.title,
                project: a.project,
                topics: a.topics,
                importance: a.importance,
                supersedes: a.supersedes,
                cwd: cwd(),
                author: alice_core::model::Author { agent: Some(a.agent), ..Default::default() },
                ..Default::default()
            };
            let r = remember_or_spool(&home, &launcher(), req).await?;
            print(json, &r, || match (&r.status, &r.id) {
                (RememberStatus::Spooled, _) => "Queued: the daemon is starting; the memory will be saved shortly.\n".into(),
                (RememberStatus::Duplicate, Some(id)) => format!("Already remembered: {id} ({})\n", r.project),
                (_, Some(id)) => format!("Saved {id} ({})\n", r.project),
                _ => "Saved.\n".into(),
            })
        }
        Command::Context(a) => {
            let r = client(&home).await?.context(&ContextRequest { project: a.project, max_tokens: a.max_tokens, cwd: cwd() }).await?;
            print(json, &r, || r.text.clone())
        }
        Command::Get(a) => {
            let r = client(&home).await?.get(&GetRequest { ids: a.ids, include_provenance: a.provenance }).await?;
            print(json, &r, || {
                let mut s = String::new();
                for m in &r.memories {
                    let m = &m.memory;
                    s.push_str(&format!("{} · {} · {} · {} · {}\n{}\n{}\n\n", m.id, m.kind.as_str(), m.project, m.status.as_str(), m.created_at, m.title, m.body));
                }
                for id in &r.missing {
                    s.push_str(&format!("not found: {id}\n"));
                }
                s
            })
        }
        Command::Update(a) => {
            let action = match a.action {
                ActionArg::Edit => UpdateAction::Edit,
                ActionArg::Supersede => UpdateAction::Supersede,
                ActionArg::Archive => UpdateAction::Archive,
                ActionArg::Forget => UpdateAction::Forget,
                ActionArg::Restore => UpdateAction::Restore,
            };
            let update = MemoryUpdate { action, title: a.title, body: a.body, kind: kind(&a.kind)?, topics: None, importance: a.importance, superseded_by: a.by };
            let r = client(&home).await?.update(&UpdateRequest { id: a.id, update }).await?;
            print(json, &r, || format!("{} is now {}\n", r.memory.id, r.memory.status.as_str()))
        }
        Command::Facts(a) => {
            let r = client(&home).await?.facts(&FactsQuery { entity: a.entity, predicate: a.predicate, as_of: a.as_of, timeline: a.timeline }).await?;
            print(json, &r, || r.text.clone())
        }
        Command::Fact(a) => {
            let action = match a.action {
                FactActionArg::Add => FactAction::Add,
                FactActionArg::Supersede => FactAction::Supersede,
                FactActionArg::End => FactAction::End,
            };
            let r = client(&home).await?.assert_fact(&AssertFact { subject: a.subject, predicate: a.predicate, object: a.object, valid_from: a.from, action, ..Default::default() }).await?;
            print(json, &r, || format!("ok ({}; ended {})\n", r.relation_id.clone().unwrap_or_else(|| "-".into()), r.ended))
        }
        Command::Projects => {
            let r = client(&home).await?.projects().await?;
            print(json, &r, || r.iter().map(|p| format!("{}\t{}\n", p.slug, p.name)).collect())
        }
        Command::Status => match DaemonClient::try_existing(&home).await {
            Some(c) => {
                let h = c.health().await?;
                print(json, &h, || {
                    format!(
                        "running (pid {}, v{})\nmemories {} · chunks {} · projects {} · embedding backlog {}{}{}\n",
                        h.pid, h.version, h.memories, h.chunks, h.projects, h.embed_backlog,
                        if h.index_ready { "" } else { " · index rebuilding" },
                        h.read_only.as_deref().map(|r| format!("\nREAD-ONLY: {r}")).unwrap_or_default()
                    )
                })
            }
            None => print(json, &serde_json::json!({"running": false}), || "not running\n".into()),
        },
        Command::Daemon(DaemonCmd::Start) => {
            let c = client(&home).await?;
            let h = c.health().await?;
            print(json, &h, || format!("running (pid {})\n", h.pid))
        }
        Command::Daemon(DaemonCmd::Stop) => match DaemonClient::try_existing(&home).await {
            Some(c) => {
                c.shutdown().await?;
                print(json, &serde_json::json!({"stopped": true}), || "stopping\n".into())
            }
            None => print(json, &serde_json::json!({"stopped": false}), || "not running\n".into()),
        },
        Command::Daemon(DaemonCmd::Status) => Box::pin(client_command(home, Command::Status, json)).await,
        Command::Daemon(DaemonCmd::Logs) => {
            let log = std::fs::read_to_string(home.logs_dir().join("daemon.log")).unwrap_or_default();
            let lines: Vec<&str> = log.lines().collect();
            for l in &lines[lines.len().saturating_sub(100)..] {
                println!("{l}");
            }
            Ok(())
        }
        Command::Daemon(DaemonCmd::Run) | Command::Mcp | Command::Model(_) | Command::Bench(_) => unreachable!("handled in dispatch"),
    }
}
```

Bench is implemented in Task 14; until then add a stub in `cmd.rs` that returns an error so the crate compiles:
```rust
pub fn bench(_home: AliceHome, _args: BenchArgs, _json: bool) -> Result<()> {
    bail!("bench lands in Task 14")
}
```

- [ ] **Step 4: Process-level tests (`crates/alice/tests/cli.rs`)**

```rust
//! Real processes: auto-start, start races, kill -9, stale discovery files, stdio MCP.
#![cfg(unix)]

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_alice");

fn alice(home: &Path, args: &[&str]) -> Output {
    Command::new(BIN).args(args).env("ALICE_HOME", home).env("ALICE_EMBEDDER", "fake").env("ALICE_LOG", "warn").output().unwrap()
}

fn ok(home: &Path, args: &[&str]) -> String {
    let o = alice(home, args);
    assert!(o.status.success(), "alice {args:?} failed: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn eventually(mut f: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("timed out waiting for {what}");
}

fn stop(home: &Path) {
    let _ = alice(home, &["daemon", "stop"]);
}

fn daemon_pid(home: &Path) -> Option<i32> {
    let raw = std::fs::read_to_string(home.join("daemon.json")).ok()?;
    serde_json::from_str::<serde_json::Value>(&raw).ok()?["pid"].as_i64().map(|p| p as i32)
}

#[test]
fn remember_auto_starts_the_daemon_and_recall_finds_it() {
    let d = tempfile::tempdir().unwrap();
    let out = ok(d.path(), &["remember", "--kind", "decision", "ship", "the", "single", "binary"]);
    assert!(out.starts_with("Saved") || out.starts_with("Queued"), "{out}");
    eventually(|| ok(d.path(), &["recall", "single", "binary"]).contains("ship the single binary"), "recall");
    assert!(ok(d.path(), &["status"]).starts_with("running"));
    let hostile = alice(d.path(), &["recall", "\"NEAR(*", "a:b"]);
    assert!(hostile.status.success());
    stop(d.path());
}

#[test]
fn ten_concurrent_first_starts_make_one_daemon_and_lose_nothing() {
    let d = tempfile::tempdir().unwrap();
    let children: Vec<_> = (0..10)
        .map(|i| Command::new(BIN).args(["remember", &format!("race memory number {i}")]).env("ALICE_HOME", d.path()).env("ALICE_EMBEDDER", "fake").env("ALICE_LOG", "warn").spawn().unwrap())
        .collect();
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }
    eventually(|| {
        let out = ok(d.path(), &["--json", "recall", "race memory number", "--limit", "20"]);
        serde_json::from_str::<serde_json::Value>(&out).unwrap()["results"].as_array().map(|a| a.len()) == Some(10)
    }, "all ten memories");
    let log = std::fs::read_to_string(d.path().join("logs/daemon.log")).unwrap_or_default();
    assert_eq!(log.matches("daemon started").count(), 1, "exactly one daemon acquired the lock:\n{log}");
    stop(d.path());
}

#[test]
fn kill_9_loses_no_acknowledged_write() {
    let d = tempfile::tempdir().unwrap();
    ok(d.path(), &["daemon", "start"]);
    for i in 0..5 {
        assert!(ok(d.path(), &["remember", &format!("durable fact {i}")]).starts_with("Saved"));
    }
    let pid = daemon_pid(d.path()).unwrap();
    // SAFETY: sending a signal to a child process we started.
    unsafe { libc_kill(pid) };
    std::thread::sleep(Duration::from_millis(300));
    ok(d.path(), &["remember", "after the crash"]);
    eventually(|| {
        let out = ok(d.path(), &["--json", "recall", "durable fact", "--limit", "20"]);
        serde_json::from_str::<serde_json::Value>(&out).unwrap()["results"].as_array().map(|a| a.len()) == Some(5)
    }, "five durable facts");
    stop(d.path());
}

unsafe fn libc_kill(pid: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid, 9) };
}

#[test]
fn stale_discovery_file_never_sends_the_token_to_a_stranger() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path()).unwrap();
    let stranger = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = stranger.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    {
        let seen = seen.clone();
        std::thread::spawn(move || {
            for mut s in stranger.incoming().flatten() {
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                seen.lock().unwrap().push_str(&String::from_utf8_lossy(&buf[..n]));
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 34\r\n\r\n{\"service\":\"x\",\"instance\":\"other\"}");
            }
        });
    }
    std::fs::write(d.path().join("daemon.json"), format!("{{\"pid\":1,\"port\":{port},\"instance\":\"STALE\",\"version\":\"0\",\"started_at\":\"t\"}}")).unwrap();
    std::fs::write(d.path().join("token"), "secret-token-value").unwrap();
    ok(d.path(), &["remember", "goes to the real daemon"]);
    eventually(|| ok(d.path(), &["recall", "real daemon"]).contains("goes to the real daemon"), "recall after stale file");
    let seen = seen.lock().unwrap().to_lowercase();
    assert!(!seen.contains("authorization"), "token leaked to a foreign process:\n{seen}");
    stop(d.path());
}

#[tokio::test(flavor = "multi_thread")]
async fn stdio_mcp_shim_lists_tools_and_round_trips() {
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
    let d = tempfile::tempdir().unwrap();
    let home = d.path().to_path_buf();
    let transport = TokioChildProcess::new(tokio::process::Command::new(BIN).configure(|c| {
        c.arg("mcp").env("ALICE_HOME", &home).env("ALICE_EMBEDDER", "fake").env("ALICE_LOG", "warn");
    }))
    .unwrap();
    let started = Instant::now();
    let client = ().serve(transport).await.unwrap();
    eprintln!("stdio connect took {:?}", started.elapsed());
    assert_eq!(client.list_all_tools().await.unwrap().len(), 7);
    let mut args = serde_json::Map::new();
    args.insert("content".into(), "stdio shim memory".into());
    let r = client.call_tool(CallToolRequestParams::new("remember").with_arguments(args)).await.unwrap();
    assert_ne!(r.is_error, Some(true));
    client.cancel().await.unwrap();
    stop(&home);
}
```

> `().serve(transport)` uses rmcp's default client handler; if it is not implemented for `()` in rmcp 3.5, copy the `TestClient` struct from `crates/alice-daemon/tests/mcp.rs`. `alice-memory` needs `tokio` with `macros` as a dev-dependency for `#[tokio::test]` (already a normal dependency).

- [ ] **Step 5: Run tests**

Run: `nice -n 15 cargo build -j 6 -p alice-memory && nice -n 15 cargo test -j 6 -p alice-memory -- --test-threads 2`
Expected: PASS. (`--test-threads 2` because each test starts a daemon.)

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/alice
git commit -m "feat(cli): alice binary with CLI, daemon entry, stdio MCP shim and process-level tests

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 14: Benchmarks, end-to-end agent checks, docs and push

**Files:**
- Create: `crates/alice/src/bench.rs`, `docs/benchmarks/m1.md`
- Modify: `crates/alice/src/{cmd.rs,main.rs}` (wire `bench`), `README.md`, `README.th.md`, `docs/superpowers/specs/2026-10-13-alice-core-design.md` (§4.2, §6, §11 facts from the spike)

**Interfaces:**
- Consumes: `alice_daemon::service::{Service, ServiceOptions}`, `alice_embed::FastEmbedder`, `alice_core::embed::FakeEmbedder`
- Produces: `alice bench [--memories N] [--chunks M] [--real] [--out FILE]` printing a table (and JSON with `--json`) with: ingest time, embedding throughput, `remember` p50/p95, `recall` p50/p95, `context` p95, peak RSS.

- [ ] **Step 1: Implement `bench.rs`**

```rust
//! Synthetic benchmark in a throw-away ALICE_HOME (never the user's data).

use crate::cmd::BenchArgs;
use alice_core::api::*;
use alice_core::embed::{Embedder, FakeEmbedder};
use alice_core::model::Kind;
use alice_core::store::sources::{ChunkInput, SourceInput};
use alice_daemon::paths::AliceHome;
use alice_daemon::service::{Service, ServiceOptions};
use anyhow::Result;
use serde::Serialize;
use std::sync::Arc;
use std::time::{Duration, Instant};

const WORDS_EN: &[&str] = &["writer", "lease", "timeout", "sqlite", "deploy", "proxy", "socks5", "protobuf", "frame", "index", "vector", "token", "daemon", "hook", "session", "agent", "memory", "decision", "bug", "fix"];
const WORDS_TH: &[&str] = &["ระบบ", "ความจำ", "เอเจนต์", "ค้นหา", "ภาษาไทย", "บันทึก", "โปรเจกต์", "แก้บั๊ก", "ตัดสินใจ", "ข้อมูล"];
const CODE: &[&str] = &["HostLinkReconnect", "writer_lease", "internal/teacher/jev/client.go", "VectorIndex::upsert", "parse_hlc"];

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 as usize
    }
    fn pick<'a>(&mut self, xs: &'a [&'a str]) -> &'a str {
        xs[self.next() % xs.len()]
    }
    fn sentence(&mut self) -> String {
        let n = 8 + self.next() % 20;
        (0..n).map(|i| match (i + self.next()) % 7 { 0 => self.pick(WORDS_TH), 1 => self.pick(CODE), _ => self.pick(WORDS_EN) }).collect::<Vec<_>>().join(" ")
    }
}

#[derive(Serialize)]
struct Report {
    embedder: String,
    memories: usize,
    chunks: usize,
    ingest_secs: f64,
    embed_per_sec: f64,
    remember_p50_ms: f64,
    remember_p95_ms: f64,
    recall_p50_ms: f64,
    recall_p95_ms: f64,
    context_p95_ms: f64,
    rss_mb: f64,
}

fn pct(v: &mut [Duration], p: f64) -> f64 {
    v.sort();
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)].as_secs_f64() * 1000.0
}

fn rss_mb() -> f64 {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output();
    out.ok().and_then(|o| String::from_utf8(o.stdout).ok()).and_then(|s| s.trim().parse::<f64>().ok()).map(|kb| kb / 1024.0).unwrap_or(0.0)
}

pub fn bench(_home: AliceHome, args: BenchArgs, json: bool) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let tmp = tempfile::tempdir()?;
    let bench_home = AliceHome::new(tmp.path().to_path_buf());
    let models = AliceHome::from_env().models_dir();
    let real = args.real;
    let make = move || -> alice_core::Result<Arc<dyn Embedder>> {
        if real { Ok(Arc::new(alice_embed::FastEmbedder::new(&models)?)) } else { Ok(Arc::new(FakeEmbedder::new(384))) }
    };
    let svc = Service::start(bench_home, &make, ServiceOptions::default())?;
    let report = rt.block_on(async {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let t = Instant::now();
        for b in 0..args.chunks.div_ceil(1_000) {
            let n = (args.chunks - b * 1_000).min(1_000);
            let chunks = (0..n).map(|i| ChunkInput { ord: (b * 1_000 + i) as i64, text: rng.sentence(), ..Default::default() }).collect();
            svc.ingest(IngestRequest { source: SourceInput { uri: format!("bench-{}", b / 50), ..Default::default() }, chunks, ..Default::default() }).await?;
        }
        for i in 0..args.memories {
            svc.remember(RememberRequest { content: format!("{} #{i}", rng.sentence()), kind: Some(Kind::ALL[i % Kind::ALL.len()]), ..Default::default() }).await?;
        }
        let ingest_secs = t.elapsed().as_secs_f64();
        let t = Instant::now();
        loop {
            let h = svc.health().await?;
            if h.embed_backlog == 0 && h.index_ready {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let embed_per_sec = (args.chunks + args.memories) as f64 / t.elapsed().as_secs_f64().max(0.001);
        let (mut w, mut r, mut c) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..200 {
            let s = rng.sentence();
            let t = Instant::now();
            svc.remember(RememberRequest { content: format!("bench write {i} {s}"), ..Default::default() }).await?;
            w.push(t.elapsed());
            let q: String = s.split(' ').take(3).collect::<Vec<_>>().join(" ");
            let t = Instant::now();
            svc.recall(RecallRequest { query: q, ..Default::default() }).await?;
            r.push(t.elapsed());
            if i % 4 == 0 {
                let t = Instant::now();
                svc.context(ContextRequest::default()).await?;
                c.push(t.elapsed());
            }
        }
        anyhow::Ok(Report {
            embedder: if real { alice_embed::MODEL_ID.into() } else { "fake".into() },
            memories: args.memories,
            chunks: args.chunks,
            ingest_secs,
            embed_per_sec,
            remember_p50_ms: pct(&mut w, 0.50),
            remember_p95_ms: pct(&mut w, 0.95),
            recall_p50_ms: pct(&mut r, 0.50),
            recall_p95_ms: pct(&mut r, 0.95),
            context_p95_ms: pct(&mut c, 0.95),
            rss_mb: rss_mb(),
        })
    })?;
    svc.shutdown();
    if let Some(path) = &args.out {
        std::fs::write(path, serde_json::to_vec_pretty(&report)?)?;
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("embedder        {}\ndata            {} memories, {} chunks\ningest          {:.1} s\nembedding       {:.0} items/s\nremember        p50 {:.2} ms · p95 {:.2} ms (budget 50)\nrecall          p50 {:.2} ms · p95 {:.2} ms (budget 200)\ncontext         p95 {:.2} ms (budget 150)\nRSS             {:.0} MB",
            report.embedder, report.memories, report.chunks, report.ingest_secs, report.embed_per_sec,
            report.remember_p50_ms, report.remember_p95_ms, report.recall_p50_ms, report.recall_p95_ms, report.context_p95_ms, report.rss_mb);
    }
    Ok(())
}
```

Wire it: `mod bench;` in `main.rs`, and in `cmd::dispatch` call `crate::bench::bench(home, args, json)` (remove the Task 13 stub). Add `tempfile` and `serde` to `alice-memory`'s normal dependencies.

- [ ] **Step 2: Run the benchmarks on the owner's machine (release)**

```bash
source "$HOME/.cargo/env"
nice -n 15 cargo build -j 6 --release -p alice-memory
./target/release/alice model fetch
nice -n 15 ./target/release/alice bench --chunks 100000 --memories 2000 --out /tmp/alice-bench-fake-100k.json
nice -n 15 ./target/release/alice bench --chunks 1000000 --memories 10000 --out /tmp/alice-bench-fake-1m.json
nice -n 15 ./target/release/alice bench --real --chunks 100000 --memories 2000 --out /tmp/alice-bench-real-100k.json
```
Expected: all budgets met (`remember` p95 < 50 ms, `recall` p95 < 200 ms, `context` p95 < 150 ms) in each run; RSS < 800 MB at 1M. Run one at a time. If a budget is missed, profile (`cargo flamegraph` or `samply`) and fix before continuing; do not change the budgets.

- [ ] **Step 3: Record results in `docs/benchmarks/m1.md`**

A table per run with the numbers from the JSON files, machine details (`sysctl -n machdep.cpu.brand_string`, cores, RAM), date, commit hash, and one paragraph on anything surprising.

- [ ] **Step 4: End-to-end checks with real agents (isolated ALICE_HOME)**

```bash
export ALICE_HOME=$(mktemp -d)
BIN=$(pwd)/target/release/alice
cat > /tmp/alice-mcp.json <<EOF
{"mcpServers":{"alice":{"command":"$BIN","args":["mcp"],"env":{"ALICE_HOME":"$ALICE_HOME"}}}}
EOF
claude -p "Use the alice MCP tools. First call remember with content 'E2E check: Alice M1 works with Claude Code' and kind fact. Then call recall with query 'E2E check Alice' and print the recalled text verbatim." \
  --mcp-config /tmp/alice-mcp.json --strict-mcp-config --allowedTools "mcp__alice__remember,mcp__alice__recall"
codex exec -c "mcp_servers.alice.command=\"$BIN\"" -c 'mcp_servers.alice.args=["mcp"]' -c "mcp_servers.alice.env={ALICE_HOME=\"$ALICE_HOME\"}" \
  "Use the alice MCP tool recall with query 'E2E check Alice' and print what it returns."
$BIN --json recall "E2E check Alice"
$BIN daemon stop
```
Expected: Claude prints the recalled sentence; Codex prints the same memory (written by Claude); the final CLI recall returns it with `author.agent` set to Claude Code's client name. Record the outputs (trimmed) in `docs/benchmarks/m1.md` under "Agent checks". If Codex is not installed or not logged in, record that and skip it; do not install or log in on the owner's behalf.

- [ ] **Step 5: Docs**

- `README.md` / `README.th.md`: change the status banner to "M1 complete — alpha"; add "Install from source" (`cargo install --git https://github.com/icueth/alice alice-memory`, then `alice model fetch`); keep packaged installers marked as planned; add a "Platform support" note: Apple Silicon macOS and glibc Linux (x86_64, aarch64) with local embeddings; Windows builds but is untested; Intel macOS and musl need an HTTP embedding provider (M2). Update the roadmap table (M1 done).
- Spec: §4.2 default model is the int8 build (`multilingual-e5-small-q8`, 118 MB model + 17 MB tokenizer, pinned revision); §6 secrets default to a 0600 file, OS keychain opt-in (keyring prompts and blocks across binary rebuilds on macOS and needs D-Bus on Linux); §11 release targets per the ORT prebuilt matrix (no musl, no Intel macOS local embeddings). Add these to §14 decisions.

- [ ] **Step 6: Full verification, commit and push**

```bash
cargo fmt --all --check
nice -n 15 cargo clippy -j 6 --workspace --all-targets -- -D warnings
nice -n 15 cargo test -j 6 --workspace -- --test-threads 2
git add -A
git commit -m "feat: M1 benchmarks, agent end-to-end checks, docs

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
git push origin main
```
Expected: all green locally; then CI green on GitHub for macOS, Linux and Windows (check with `gh run watch`).
