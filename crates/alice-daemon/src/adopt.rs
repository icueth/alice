//! `alice adopt <file>` (server spec §2.12, addendum 4): a person's own Alice database, made on
//! their machine with `alice backup now`, becomes a fresh server tenant's.
//!
//! The file is checked first (a full `integrity_check`, Alice's tables, and exactly this build's
//! schema), the tenant must hold nothing yet, and then it goes in through `alice restore`'s staged,
//! atomic swap: the tenant's own empty database is kept as a before-restore backup and the vector
//! index is dropped (rebuilt at the next start from the stored embeddings). In one transaction the
//! person's machine paths are then scrubbed and the tenant's redaction rules applied again; if that
//! fails, the tenant's own database is put back, so an unscrubbed copy is never served. Settings,
//! secrets and the token stay the tenant's own: only `alice.db` is replaced.
//!
//! No daemon may run for the tenant (`alice-server adopt` stops its unit first; the swap refuses
//! while a daemon holds the lock).

use crate::config::SettingsHandle;
use crate::paths::AliceHome;
use crate::redaction::Redactors;
use crate::{backup, restore};
use alice_core::error::{AliceError, Result};
use alice_core::ids::Hlc;
use alice_core::normalize::sha256_hex;
use alice_core::redact::Redactor;
use alice_core::store::events::{self, Actor, EventInput};
use alice_core::store::memories::{self, MemoryUpdate, UpdateAction};
use alice_core::store::{fts, history, migrations};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// A project key made from its working tree's path (`path:<root>`), as a git-less checkout gets.
const PATH_KEY_PREFIX: &str = "path:";
/// What such a key and a source's path become: a hash keeps them unique without the path.
const ADOPTED_PREFIX: &str = "adopted:";
/// The event a chunk redacted in place records (never undone: the old text held the secret).
pub const CHUNK_REDACT_ACTION: &str = "chunk.redact";
/// How an error starts when the adopted, unscrubbed database is still in place: `alice-server`
/// then leaves the tenant stopped.
pub const NOT_PUT_BACK: &str = "[not put back]";
/// How an error starts when the tenant's database was never touched (a refusal before the swap).
pub const UNTOUCHED: &str = "[untouched]";
/// How an error starts when the tenant's own database is back after a failed adoption.
pub const PUT_BACK: &str = "[put back]";
// `alice-server` starts the tenant again only after one of the last two: any other end (a panic,
// a kill) may have left the unscrubbed copy in place, so it stays stopped.

/// What `alice adopt` did.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct AdoptReport {
    pub adopted_from: String,
    pub memories: u64,
    pub chunks: u64,
    pub facts: u64,
    pub projects: u64,
    /// Project roots, path-made project keys and source paths of the person's machine, cleared.
    pub paths_cleared: u64,
    /// The projects that had a working tree on the person's machine (comma-separated slugs): the
    /// ones whose `.alice.toml` may have kept them from the cloud there. Their rules do not come
    /// along; `alice-server adopt --cloud-denied` carries such a list.
    pub projects_with_roots: String,
    /// Events and review items whose snapshots named one of those paths, rewritten.
    pub records_rewritten: u64,
    /// Memories and raw chunks the tenant's redaction rules changed (each with an event).
    pub redacted_memories: u64,
    pub redacted_chunks: u64,
    /// Of those memories, the ones whose redacted text equals another memory's: trashed (they
    /// stay in the trash, redacted, and point at nothing).
    pub duplicates_trashed: u64,
    /// History snapshots and review items the rules changed.
    pub redacted_records: u64,
    /// The tenant's own database before the adoption, in its backups directory.
    pub saved_tenant_db_as: Option<String>,
    pub secs: f64,
}

/// Check a person's backup before anything changes: a SQLite file that passes `integrity_check`,
/// with Alice's tables and exactly this build's schema (an older one would be migrated on the
/// server with no way back to the person's machine; a newer one cannot be read).
pub fn check_source(path: &Path) -> Result<restore::VerifiedBackup> {
    let shown = path.display();
    let conn = restore::open_ro(path)
        .map_err(|e| AliceError::invalid(format!("{shown} is not an Alice database: {e}")))?;
    let version: Option<i64> = conn
        .query_row("SELECT max(version) FROM schema_migrations", [], |r| {
            r.get(0)
        })
        .optional()
        .ok()
        .flatten()
        .flatten();
    let version = version.ok_or_else(|| {
        AliceError::invalid(format!(
            "{shown} is not an Alice database (no schema version)"
        ))
    })?;
    let current = migrations::CURRENT_VERSION;
    if version < current {
        return Err(AliceError::invalid(format!(
            "{shown} was made by an older Alice (schema v{version}; the server runs v{current}): upgrade the source Alice first, run `alice backup now` again and copy the new file"
        )));
    }
    if version > current {
        return Err(AliceError::invalid(format!(
            "{shown} was made by a newer Alice (schema v{version}; the server runs v{current}): upgrade the server first (alice-server upgrade)"
        )));
    }
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(|e| AliceError::invalid(format!("{shown} could not be checked: {e}")))?;
    if integrity != "ok" {
        return Err(AliceError::invalid(format!(
            "{shown} fails SQLite's integrity check ({integrity}); make a new backup on the source machine"
        )));
    }
    drop(conn);
    restore::verify(path)
}

/// Refuse a tenant that already holds anything: adopting would replace it.
pub fn check_fresh(home: &AliceHome) -> Result<()> {
    let db = home.db();
    if !db.exists() {
        return Ok(());
    }
    let conn = Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let count = |table: &str| -> Result<u64> {
        Ok(
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
                r.get::<_, i64>(0)
            })? as u64,
        )
    };
    let (m, c, f) = (count("memories")?, count("chunks")?, count("relations")?);
    if m + c + f > 0 {
        return Err(AliceError::invalid(format!(
            "this tenant already holds {m} memories, {c} chunks and {f} facts; adopt needs a fresh tenant (alice-server remove it, then add it again)"
        )));
    }
    Ok(())
}

/// Adopt `path` into `home`'s tenant: see the module doc. The tenant's settings decide the
/// redaction rules (always on, as on any tenant) and the backups directory.
pub fn adopt(home: &AliceHome, path: &Path, now: DateTime<Utc>) -> Result<AdoptReport> {
    let started = std::time::Instant::now();
    let untouched = |e: AliceError| AliceError::Other(format!("{UNTOUCHED} {e}"));
    let verified = check_source(path).map_err(untouched)?;
    check_fresh(home).map_err(untouched)?;
    let settings = SettingsHandle::load_tenant(home);
    let redactor = Redactors::new(settings.clone()).for_root_always_on(None);
    let dir = backup::backup_dir(home, &settings.get().backup);
    // A swap that fails leaves `alice.db` as it was (restore's invariant).
    let swapped = restore::swap_verified(home, &dir, &verified, now).map_err(untouched)?;
    let mut report = match finish(home, &redactor) {
        Ok(r) => r,
        Err(e) => {
            // Never leave an unscrubbed copy to be served: the tenant's own database goes back.
            let back = swapped
                .saved_current_as
                .as_ref()
                .map(|saved| restore::swap(home, &dir, &dir.join(saved), now));
            return Err(match back {
                Some(Ok(put_back)) => {
                    // The swap back kept the adopted file, unscrubbed, as a before-restore copy.
                    if let Some(copy) = put_back.saved_current_as {
                        let _ = std::fs::remove_file(dir.join(copy));
                    }
                    AliceError::Other(format!(
                        "{PUT_BACK} adopting failed, the tenant's own database is back: {e}"
                    ))
                }
                Some(Err(b)) => AliceError::Other(format!(
                    "{NOT_PUT_BACK} adopting failed ({e}), and the tenant's own database could not be put back ({b}); restore it with `alice restore {}` before the tenant starts",
                    swapped.saved_current_as.as_deref().unwrap_or("")
                )),
                None => AliceError::Other(format!("adopting failed: {e}")),
            });
        }
    };
    report.adopted_from = path.display().to_string();
    report.saved_tenant_db_as = swapped.saved_current_as;
    report.secs = started.elapsed().as_secs_f64();
    Ok(report)
}

/// [`adopt`] stamped with the time of the call.
pub fn adopt_now(home: &AliceHome, path: &Path) -> Result<AdoptReport> {
    adopt(home, path, Utc::now())
}

/// Scrub and redact the adopted database, in one transaction, and count what it holds.
fn finish(home: &AliceHome, redactor: &Arc<Redactor>) -> Result<AdoptReport> {
    let mut conn = alice_core::store::open_writer(&home.db())?;
    // Text replaced below must not stay behind in freed pages of the file.
    conn.pragma_update(None, "secure_delete", "ON")?;
    let tx = conn.transaction()?;
    let mut r = AdoptReport::default();
    scrub_paths(&tx, &mut r)?;
    redact_all(&tx, redactor, &mut r)?;
    // A contentless FTS5 table keeps a deleted row's terms in its segments until they merge: the
    // text replaced above, and whatever the person deleted on their machine, would stay readable.
    for t in ["memories_fts", "memories_tri", "chunks_fts", "chunks_tri"] {
        tx.execute(&format!("INSERT INTO {t}({t}) VALUES('optimize')"), [])?;
    }
    let count = |table: &str| -> Result<u64> {
        Ok(
            tx.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })? as u64,
        )
    };
    r.memories = count("memories")?;
    r.chunks = count("chunks")?;
    r.facts = count("relations")?;
    r.projects = count("projects")?;
    tx.commit()?;
    // The file is rewritten: free pages (the person's deleted rows ride along in a backup) and
    // the pages of replaced text are gone, then the WAL goes into the main file.
    conn.execute_batch("VACUUM")?;
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    Ok(r)
}

/// Whether a source URI names a file on the person's machine.
fn is_client_path(uri: &str) -> bool {
    let b = uri.as_bytes();
    uri.starts_with('/')
        || uri.starts_with('~')
        || uri.starts_with("file:")
        || uri.starts_with("\\\\")
        || (b.len() > 2
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && matches!(b[2], b'\\' | b'/'))
}

fn adopted(old: &str) -> String {
    format!("{ADOPTED_PREFIX}{}", &sha256_hex(old.as_bytes())[..32])
}

/// Clear what names the person's machine: project roots, path-made project keys (and the keys of
/// projects merged into others), source paths (a hash and the file's own name stay), and the
/// transcript paths and file identities of captured sessions. JSON snapshots in events and review
/// items that held one of the renamed values get the new one.
fn scrub_paths(tx: &Connection, r: &mut AdoptReport) -> Result<()> {
    let mut renamed: HashMap<String, serde_json::Value> = HashMap::new();
    let roots: Vec<(String, String)> = tx
        .prepare("SELECT slug, root_path FROM projects WHERE root_path IS NOT NULL ORDER BY slug")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    r.projects_with_roots = roots
        .iter()
        .map(|(slug, _)| slug.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    for (_, root) in roots {
        renamed.insert(root, serde_json::Value::Null);
    }
    r.paths_cleared += tx.execute(
        "UPDATE projects SET root_path = NULL WHERE root_path IS NOT NULL",
        [],
    )? as u64;
    for table in ["projects", "project_key_aliases"] {
        let keys: Vec<String> = tx
            .prepare(&format!(
                "SELECT key FROM {table} WHERE substr(key, 1, {}) = ?1",
                PATH_KEY_PREFIX.len()
            ))?
            .query_map([PATH_KEY_PREFIX], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for key in keys {
            let new = adopted(&key);
            tx.execute(
                &format!("UPDATE {table} SET key = ?1 WHERE key = ?2"),
                params![new, key],
            )?;
            renamed.insert(key, serde_json::Value::String(new));
            r.paths_cleared += 1;
        }
    }
    let sources: Vec<(String, String)> = tx
        .prepare("SELECT id, uri FROM sources")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, uri) in sources.into_iter().filter(|(_, u)| is_client_path(u)) {
        let name = uri
            .rsplit(['/', '\\'])
            .next()
            .filter(|n| !n.is_empty())
            .unwrap_or("source");
        let new = format!("{}/{name}", adopted(&uri));
        tx.execute(
            "UPDATE sources SET uri = ?1 WHERE id = ?2",
            params![new, id],
        )?;
        renamed.insert(uri, serde_json::Value::String(new));
        r.paths_cleared += 1;
    }
    // A captured session's state (SP2) names its transcript file: cleared here, inside this
    // transaction, so the VACUUM after it leaves no copy in the file.
    r.paths_cleared += crate::capture::forget_local_paths(tx)? as u64;
    if renamed.is_empty() {
        return Ok(());
    }
    for (table, key, columns) in [
        ("events", "id", &["before_json", "after_json"][..]),
        ("review_items", "id", &["payload_json"][..]),
    ] {
        for column in columns {
            let rows: Vec<(String, String)> = tx
                .prepare(&format!(
                    "SELECT {key}, {column} FROM {table} WHERE {column} IS NOT NULL"
                ))?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (id, text) in rows {
                let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                if replace_values(&mut v, &renamed) {
                    tx.execute(
                        &format!("UPDATE {table} SET {column} = ?1 WHERE {key} = ?2"),
                        params![v.to_string(), id],
                    )?;
                    r.records_rewritten += 1;
                }
            }
        }
    }
    Ok(())
}

/// Replace every string in `v` that is a key of `renamed` by its value; true when one was.
fn replace_values(v: &mut serde_json::Value, renamed: &HashMap<String, serde_json::Value>) -> bool {
    match v {
        serde_json::Value::String(s) => match renamed.get(s.as_str()) {
            Some(new) => {
                *v = new.clone();
                true
            }
            None => false,
        },
        serde_json::Value::Array(items) => items
            .iter_mut()
            .fold(false, |acc, i| replace_values(i, renamed) | acc),
        serde_json::Value::Object(map) => map
            .values_mut()
            .fold(false, |acc, i| replace_values(i, renamed) | acc),
        _ => false,
    }
}

/// Apply the tenant's redaction rules to every memory's title and body and every raw chunk; a
/// memory changes through the ordinary edit (its event's snapshots are redacted too, so the old
/// text is not kept), a chunk in place with a `chunk.redact` event. Both are embedded again.
fn redact_all(tx: &Connection, redactor: &Arc<Redactor>, r: &mut AdoptReport) -> Result<()> {
    let mut hlc = Hlc::new("adopt");
    let rules = |_: &Connection, _: Option<&str>| redactor.clone();
    let memories: Vec<(String, String, String)> = tx
        .prepare("SELECT id, title, body FROM memories")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, title, body) in memories {
        let (t, b) = (redactor.apply(&title), redactor.apply(&body));
        if t.found.is_empty() && b.found.is_empty() {
            continue;
        }
        let mut before = history::snapshot_memory(tx, &id)?
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| AliceError::Other(format!("memory snapshot: {e}")))?;
        let update = MemoryUpdate {
            action: UpdateAction::Edit,
            title: (!t.found.is_empty()).then_some(t.text),
            body: (!b.found.is_empty()).then_some(b.text),
            ..Default::default()
        };
        if let Err(e) = memories::apply_update(tx, &id, &update, hlc.tick()) {
            // Its text without the secret is another memory's: trash it, then redact it.
            if !e.to_string().contains("already holds this text") {
                return Err(e);
            }
            let forget = MemoryUpdate {
                action: UpdateAction::Forget,
                ..Default::default()
            };
            memories::apply_update(tx, &id, &forget, hlc.tick())?;
            memories::apply_update(tx, &id, &update, hlc.tick())?;
            r.duplicates_trashed += 1;
        }
        let mut after = history::snapshot_memory(tx, &id)?
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| AliceError::Other(format!("memory snapshot: {e}")))?;
        history::scrub_snapshots(tx, &rules, &mut before, &mut after);
        events::record(
            tx,
            &EventInput {
                actor: &Actor::System,
                action: "memory.edit",
                target_type: "memory",
                target_id: &id,
                before,
                after,
                group: None,
            },
            &hlc.tick(),
        )?;
        r.redacted_memories += 1;
    }
    let chunks: Vec<(i64, String, String)> = tx
        .prepare("SELECT seq, id, text FROM chunks")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (seq, id, text) in chunks {
        let red = redactor.apply(&text);
        if red.found.is_empty() {
            continue;
        }
        tx.execute(
            "UPDATE chunks SET text = ?1, content_hash = ?2, embed_state = 'pending' WHERE seq = ?3",
            params![red.text, sha256_hex(red.text.as_bytes()), seq],
        )?;
        fts::unindex_chunk(tx, seq)?;
        fts::index_chunk(tx, seq, &red.text)?;
        tx.execute(
            "DELETE FROM embeddings WHERE owner_type = 'chunk' AND owner_seq = ?1",
            [seq],
        )?;
        events::record(
            tx,
            &EventInput {
                actor: &Actor::System,
                action: CHUNK_REDACT_ACTION,
                target_type: "chunk",
                target_id: &id,
                before: None,
                after: Some(serde_json::json!({ "found": red.found })),
                group: None,
            },
            &hlc.tick(),
        )?;
        r.redacted_chunks += 1;
    }
    // History and the review inbox keep snapshots of what the person's machine stored.
    for (table, key, column) in [
        ("events", "id", "before_json"),
        ("events", "id", "after_json"),
        ("review_items", "id", "payload_json"),
    ] {
        let rows: Vec<(String, String)> = tx
            .prepare(&format!(
                "SELECT {key}, {column} FROM {table} WHERE {column} IS NOT NULL"
            ))?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, text) in rows {
            let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            if !redactor.apply_json(&mut v).is_empty() {
                tx.execute(
                    &format!("UPDATE {table} SET {column} = ?1 WHERE {key} = ?2"),
                    params![v.to_string(), id],
                )?;
                r.redacted_records += 1;
            }
        }
    }
    // Judge answers cached on the person's machine (they may quote what was graded).
    tx.execute("DELETE FROM judge_cache", [])?;
    Ok(())
}
